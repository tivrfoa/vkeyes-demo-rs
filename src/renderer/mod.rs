use std::sync::Arc;
use err_derive::Error;
use vulkano::{app_info_from_cargo_toml, OomError};
use vulkano::device::{Device, DeviceExtensions, RawDeviceExtensions, Features, Queue, DeviceCreationError};
use vulkano::instance::debug::{DebugCallback, MessageSeverity, MessageType};
use vulkano::instance::{Instance, InstanceExtensions, RawInstanceExtensions, PhysicalDevice, LayersListError, InstanceCreationError};
use vulkano::pipeline::{GraphicsPipeline, GraphicsPipelineCreationError};
use vulkano::sync::{GpuFuture, FlushError};
use vulkano::sync;
use vulkano::pipeline::viewport::Viewport;
use vulkano::framebuffer::{Subpass, RenderPassCreationError, RenderPassAbstract};
use vulkano::command_buffer::{AutoCommandBufferBuilder, DynamicState, BeginRenderPassError, AutoCommandBufferBuilderContextError, BuildError, CommandBufferExecError, DrawIndexedError};
use vulkano::format::ClearValue;
use openvr::{System, Compositor};
use cgmath::{Matrix4, Transform, Matrix, Vector2, Euler, Rad};
use openvr::compositor::CompositorError;

pub mod model;
mod eye;

use crate::shaders;
use crate::openvr_vulkan::*;
use crate::renderer::eye::EyeCreationError;
use crate::renderer::model::Model;
use eye::Eye;

// workaround https://github.com/vulkano-rs/vulkano/issues/709
type PipelineType = GraphicsPipeline<
	vulkano::pipeline::vertex::SingleBufferDefinition<model::Vertex>,
	std::boxed::Box<dyn vulkano::descriptor::pipeline_layout::PipelineLayoutAbstract + Send + Sync>,
	std::sync::Arc<dyn RenderPassAbstract + Send + Sync>
>;

pub struct Renderer {
	pub instance: Arc<Instance>,
	
	device: Arc<Device>,
	queue: Arc<Queue>,
	load_queue: Arc<Queue>,
	pipeline: Arc<PipelineType>,
	eyes: (Eye, Eye),
	compositor: Compositor,
	previous_frame_end: Option<Box<dyn GpuFuture>>,
}

// Translates OpenGL projection matrix to Vulkan
const CLIP: Matrix4<f32> = Matrix4::new(
	1.0, 0.0, 0.0, 0.0,
	0.0,-1.0, 0.0, 0.0,
	0.0, 0.0, 0.5, 0.0,
	0.0, 0.0, 0.5, 1.0,
);

impl Renderer {
	pub fn new(system: &System, compositor: Compositor, device: Option<usize>, debug: bool) -> Result<Renderer, RendererCreationError> {
		let recommended_size = system.recommended_render_target_size();
		
		if debug {
			println!("List of Vulkan debugging layers available to use:");
			let layers = vulkano::instance::layers_list()?;
			for layer in layers {
				println!("\t{}", layer.name());
			}
		}
		
		let instance = {
			let app_infos = app_info_from_cargo_toml!();
			let extensions = RawInstanceExtensions::new(compositor.vulkan_instance_extensions_required())
			                                       .union(&(&InstanceExtensions { ext_debug_utils: debug,
			                                                                      ..InstanceExtensions::none() }).into());
			
			let layers = if debug {
				             vec!["VK_LAYER_LUNARG_standard_validation"]
			             } else {
				             vec![]
			             };
			
			Instance::new(Some(&app_infos), extensions, layers)?
		};
		
		if debug {
			let severity = MessageSeverity { error:       true,
			                                 warning:     true,
			                                 information: false,
			                                 verbose:     true, };
			
			let ty = MessageType::all();
			
			let _debug_callback = DebugCallback::new(&instance, severity, ty, |msg| {
				                                         let severity = if msg.severity.error {
					                                         "error"
				                                         } else if msg.severity.warning {
					                                         "warning"
				                                         } else if msg.severity.information {
					                                         "information"
				                                         } else if msg.severity.verbose {
					                                         "verbose"
				                                         } else {
					                                         panic!("no-impl");
				                                         };
				                                         
				                                         let ty = if msg.ty.general {
					                                         "general"
				                                         } else if msg.ty.validation {
					                                         "validation"
				                                         } else if msg.ty.performance {
					                                         "performance"
				                                         } else {
					                                         panic!("no-impl");
				                                         };
				                                         
				                                         println!("{} {} {}: {}",
				                                                  msg.layer_prefix,
				                                                  ty,
				                                                  severity,
				                                                  msg.description);
			                                         });
		}
		
		if debug {
			println!("Devices:");
			for device in PhysicalDevice::enumerate(&instance) {
				println!("\t{}: {} api: {} driver: {}",
				         device.index(),
				         device.name(),
				         device.api_version(),
				         device.driver_version());
			}
		}
		
		let physical = system.vulkan_output_device(instance.as_ptr())
		                     .and_then(|ptr| PhysicalDevice::enumerate(&instance).find(|physical| physical.as_ptr() == ptr))
		                     .or_else(|| {
			                     println!("Failed to fetch device from openvr, using fallback");
			                     PhysicalDevice::enumerate(&instance).skip(device.unwrap_or(0)).next()
		                     })
		                     .ok_or(RendererCreationError::NoDevices)?;
		
		println!("\nUsing {}: {} api: {} driver: {}",
		         physical.index(),
		         physical.name(),
		         physical.api_version(),
		         physical.driver_version());
		
		if debug {
			for family in physical.queue_families() {
				println!("Found a queue family with {:?} queue(s)", family.queues_count());
			}
		}
		
		let (device, mut queues) = {
			let queue_family = physical.queue_families()
			                           .find(|&q| q.supports_graphics())
			                           .ok_or(RendererCreationError::NoQueue)?;
			
			let load_queue_family = physical.queue_families()
			                                .find(|&q| q.explicitly_supports_transfers())
			                                .unwrap_or(queue_family);
			
			let families = vec![
				(queue_family, 0.5),
				(load_queue_family, 0.2),
			];
			
			Device::new(physical,
			            &Features::none(),
			            RawDeviceExtensions::new(vulkan_device_extensions_required(&compositor, &physical))
			                                .union(&(&DeviceExtensions { khr_swapchain: true,
			                                                             ..DeviceExtensions::none() }).into()),
			            families.into_iter())?
		};
		
		let queue = queues.next().ok_or(RendererCreationError::NoQueue)?;
		let load_queue = queues.next().ok_or(RendererCreationError::NoQueue)?;
		
		let vs = shaders::vert::Shader::load(device.clone()).unwrap();
		let fs = shaders::frag::Shader::load(device.clone()).unwrap();
		
		let render_pass = Arc::new(
			vulkano::single_pass_renderpass!(device.clone(),
				attachments: {
					color: {
						load: Clear,
						store: Store,
						format: eye::IMAGE_FORMAT,
						samples: 1,
					},
					depth: {
						load: Clear,
						store: DontCare,
						format: eye::DEPTH_FORMAT,
						samples: 1,
					}
				},
				pass: {
					color: [color],
					depth_stencil: {depth}
				}
			)?
		);
		
		let pipeline = Arc::new(
			GraphicsPipeline::start()
			                 .vertex_input_single_buffer::<model::Vertex>()
			                 .vertex_shader(vs.main_entry_point(), ())
			                 .viewports(Some(Viewport { origin: [0.0, 0.0],
			                                            dimensions: [recommended_size.0 as f32, recommended_size.1 as f32],
			                                            depth_range: 0.0 .. 1.0 }))
			                 .fragment_shader(fs.main_entry_point(), ())
			                 .depth_stencil_simple_depth()
			                 .render_pass(Subpass::from(render_pass.clone() as Arc<dyn RenderPassAbstract + Send + Sync>, 0).unwrap())
			                 .build(device.clone())?
		);
		
		let eyes = {
			let proj_left : Matrix4<f32> = CLIP
			                             * Matrix4::from(system.projection_matrix(openvr::Eye::Left,  0.1, 1000.1)).transpose()
			                             * mat4(&system.eye_to_head_transform(openvr::Eye::Left )).inverse_transform().unwrap();
			let proj_right: Matrix4<f32> = CLIP
			                             * Matrix4::from(system.projection_matrix(openvr::Eye::Right, 0.1, 1000.1)).transpose()
			                             * mat4(&system.eye_to_head_transform(openvr::Eye::Right)).inverse_transform().unwrap();
			
			(
				Eye::new(recommended_size, proj_left,  &queue, &render_pass)?,
				Eye::new(recommended_size, proj_right, &queue, &render_pass)?,
			)
		};
		
		let previous_frame_end = Some(Box::new(sync::now(device.clone())) as Box<_>);
		
		Ok(Renderer {
			instance,
			device,
			queue,
			load_queue,
			pipeline,
			eyes,
			compositor,
			previous_frame_end,
		})
	}
	
	pub fn render(&mut self, hmd_pose: &[[f32; 4]; 3], eye_rotation: (Vector2<f32>, Vector2<f32>), scene: &mut [(Model, Matrix4<f32>)]) -> Result<(), RenderError> {
		self.previous_frame_end.as_mut().unwrap().cleanup_finished();
		
		let left_pv = self.eyes.0.projection
		            * Matrix4::from(Euler { x: Rad(eye_rotation.0.x),
		                                    y: Rad(eye_rotation.0.y),
		                                    z: Rad(0.0) })
		            * mat4(hmd_pose).inverse_transform().unwrap();
		
		let right_pv = self.eyes.1.projection
		             * Matrix4::from(Euler { x: Rad(eye_rotation.1.x),
		                                     y: Rad(eye_rotation.1.y),
		                                     z: Rad(0.0) })
		             * mat4(hmd_pose).inverse_transform().unwrap();
		
		let mut command_buffer = AutoCommandBufferBuilder::new(self.device.clone(), self.queue.family())?
		                                                  .begin_render_pass(self.eyes.0.frame_buffer.clone(),
		                                                                     false,
		                                                                     vec![ [0.5, 0.5, 0.5, 1.0].into(),
		                                                                           ClearValue::Depth(1.0) ])?;
		
		for (model, matrix) in scene.iter_mut() {
			if !model.loaded() { continue };
			command_buffer = command_buffer.draw_indexed(self.pipeline.clone(),
			                                             &DynamicState::none(),
			                                             model.vertices.clone(),
			                                             model.indices.clone(),
			                                             model.set.clone(),
			                                             left_pv * *matrix)?;
		}
		
		command_buffer = command_buffer.end_render_pass()?
		                               .begin_render_pass(self.eyes.1.frame_buffer.clone(),
		                                                  false,
		                                                  vec![ [0.5, 0.5, 0.5, 1.0].into(),
		                                                        ClearValue::Depth(1.0) ])?;
		
		for (model, matrix) in scene.iter_mut() {
			if !model.loaded() { continue };
			command_buffer = command_buffer.draw_indexed(self.pipeline.clone(),
			                                             &DynamicState::none(),
			                                             model.vertices.clone(),
			                                             model.indices.clone(),
			                                             model.set.clone(),
			                                             right_pv * *matrix)?;
		}
		
		let command_buffer = command_buffer.end_render_pass()?
		                                   .build()?;
		
		let future = self.previous_frame_end.take()
		                                    .unwrap()
		                                    .then_execute(self.queue.clone(), command_buffer)?;
		
		unsafe {
			self.compositor.submit(openvr::Eye::Left,  &self.eyes.0.texture, None, Some(hmd_pose.clone()))?;
			self.compositor.submit(openvr::Eye::Right, &self.eyes.1.texture, None, Some(hmd_pose.clone()))?;
		}
		
		let future = future.then_signal_fence_and_flush();
		
		match future {
			Ok(future) => {
				self.previous_frame_end = Some(Box::new(future) as Box<_>);
			},
			Err(FlushError::OutOfDate) => {
				eprintln!("Flush Error: Out of date, ignoring");
				self.previous_frame_end = Some(Box::new(sync::now(self.device.clone())) as Box<_>);
			},
			Err(err) => return Err(err.into()),
		}
		
		Ok(())
	}
}


#[derive(Debug, Error)]
pub enum RendererCreationError {
	#[error(display = "No devices available.")] NoDevices,
	#[error(display = "No compute queue available.")] NoQueue,
	#[error(display = "{}", _0)] LayersListError(#[error(source)] LayersListError),
	#[error(display = "{}", _0)] InstanceCreationError(#[error(source)] InstanceCreationError),
	#[error(display = "{}", _0)] DeviceCreationError(#[error(source)] DeviceCreationError),
	#[error(display = "{}", _0)] OomError(#[error(source)] OomError),
	#[error(display = "{}", _0)] RenderPassCreationError(#[error(source)] RenderPassCreationError),
	#[error(display = "{}", _0)] GraphicsPipelineCreationError(#[error(source)] GraphicsPipelineCreationError),
	#[error(display = "{}", _0)] EyeCreationError(#[error(source)] EyeCreationError),
}

#[derive(Debug, Error)]
pub enum RenderError {
	#[error(display = "{}", _0)] OomError(#[error(source)] OomError),
	#[error(display = "{}", _0)] BeginRenderPassError(#[error(source)] BeginRenderPassError),
	#[error(display = "{}", _0)] DrawIndexedError(#[error(source)] DrawIndexedError),
	#[error(display = "{}", _0)] AutoCommandBufferBuilderContextError(#[error(source)] AutoCommandBufferBuilderContextError),
	#[error(display = "{}", _0)] BuildError(#[error(source)] BuildError),
	#[error(display = "{}", _0)] CommandBufferExecError(#[error(source)] CommandBufferExecError),
	#[error(display = "{}", _0)] CompositorError(#[error(source)] CompositorError),
	#[error(display = "{}", _0)] FlushError(#[error(source)] FlushError),
}
