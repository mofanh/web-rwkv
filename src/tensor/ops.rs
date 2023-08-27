use half::f16;
use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BufferAddress, CommandEncoder, ComputePass,
    ComputePipeline,
};

use super::{Kind, ReadWrite, Shape, TensorError, TensorExt, TensorGpu, TensorView, Uniform};
use crate::num::Scalar;

pub trait TensorCommand<T: Scalar, K: Kind> {
    fn copy_tensor(
        &mut self,
        source: &TensorGpu<T, ReadWrite>,
        destination: &TensorGpu<T, K>,
    ) -> Result<(), TensorError>;
}

impl<T: Scalar, K: Kind> TensorCommand<T, K> for CommandEncoder {
    fn copy_tensor(
        &mut self,
        source: &TensorGpu<T, ReadWrite>,
        destination: &TensorGpu<T, K>,
    ) -> Result<(), TensorError> {
        source.check_shape(destination.shape())?;
        let size = source.size() as BufferAddress;
        self.copy_buffer_to_buffer(&source.buffer, 0, &destination.buffer, 0, size);
        Ok(())
    }
}

pub trait TensorPass<'a> {
    fn execute_tensor_op(&mut self, op: &'a TensorOp);
}

impl<'b, 'a: 'b> TensorPass<'a> for ComputePass<'b> {
    fn execute_tensor_op(&mut self, op: &'a TensorOp) {
        self.set_pipeline(op.pipeline);
        op.bindings
            .iter()
            .enumerate()
            .for_each(|(index, bind_group)| self.set_bind_group(index as u32, bind_group, &[]));
        self.dispatch_workgroups(op.dispatch[0], op.dispatch[1], op.dispatch[2]);
    }
}

pub struct TensorOp<'a> {
    pub pipeline: &'a ComputePipeline,
    pub bindings: Vec<BindGroup>,
    pub dispatch: [u32; 3],
}

impl<'a> TensorOp<'a> {
    const BLOCK_SIZE: u32 = 128;

    fn block_count(x: u32) -> u32 {
        (x + Self::BLOCK_SIZE - 1) / Self::BLOCK_SIZE
    }

    /// Softmax operator applied on `x`.
    pub fn softmax(x: &'a TensorGpu<f32, ReadWrite>) -> Result<Self, TensorError> {
        let shape = x.shape();
        let context = x.context();
        let pipeline = context.pipeline("softmax")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: x.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: x.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [1, shape[1] as u32, shape[2] as u32],
        })
    }

    /// Layer norm applied on `x`, with weight `w` and bias `b`.
    pub fn layer_norm(
        w: &'a TensorGpu<f16, ReadWrite>,
        b: &'a TensorGpu<f16, ReadWrite>,
        x: &'a TensorGpu<f32, ReadWrite>,
    ) -> Result<Self, TensorError> {
        let shape = x.shape();
        w.check_shape(Shape::new(shape[0], 1, 1))?;
        b.check_shape(Shape::new(shape[0], 1, 1))?;

        let context = x.context;
        let pipeline = context.pipeline("layer_norm")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: x.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: w.binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: b.binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: x.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [1, shape[1] as u32, shape[2] as u32],
        })
    }

    /// Fp16 matrix multiplication.
    /// - `matrix` shape: `[C, R, 1]`.
    /// - `input` shape: `[C, T, B]`.
    /// - `output` shape: `[R, T, B]`.
    pub fn matmul(
        matrix: &'a TensorGpu<f16, ReadWrite>,
        input: TensorView<'a, f32>,
        output: TensorView<'a, f32>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape();
        matrix.check_shape(Shape::new(input.shape()[0], shape[0], 1))?;
        input.check_shape(Shape::new(matrix.shape[0], shape[1], shape[2]))?;

        let context = output.context;
        let pipeline = context.pipeline("matmul")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: matrix.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: input.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: matrix.binding(),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: input.binding(),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [matrix.shape[1] as u32 / 4, shape[1] as u32, shape[2] as u32],
        })
    }

    /// Int8 matrix multiplication.
    /// - `matrix` shape: `[C, R, 1]`.
    /// - `mx` and `rx` shape: `[C, 1, 1]`.
    /// - `my` and `ry` shape: `[R, 1, 1]`.
    /// - `input` shape: `[C, T, B]`.
    /// - `output` shape: `[R, T, B]`.
    pub fn matmul_int8(
        matrix: &'a TensorGpu<u8, ReadWrite>,
        mx: &'a TensorGpu<f16, ReadWrite>,
        rx: &'a TensorGpu<f16, ReadWrite>,
        my: &'a TensorGpu<f16, ReadWrite>,
        ry: &'a TensorGpu<f16, ReadWrite>,
        input: TensorView<'a, f32>,
        output: TensorView<'a, f32>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape();
        matrix.check_shape(Shape::new(input.shape()[0], shape[0], 1))?;
        input.check_shape(Shape::new(matrix.shape[0], shape[1], shape[2]))?;
        mx.check_shape(Shape::new(matrix.shape[0], 1, 1))?;
        rx.check_shape(Shape::new(matrix.shape[0], 1, 1))?;
        my.check_shape(Shape::new(matrix.shape[1], 1, 1))?;
        ry.check_shape(Shape::new(matrix.shape[1], 1, 1))?;

        let context = output.context;
        let pipeline = context.pipeline("matmul_int8")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: matrix.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: input.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: matrix.binding(),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: mx.binding(),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: rx.binding(),
                },
                BindGroupEntry {
                    binding: 6,
                    resource: my.binding(),
                },
                BindGroupEntry {
                    binding: 7,
                    resource: ry.binding(),
                },
                BindGroupEntry {
                    binding: 8,
                    resource: input.binding(),
                },
                BindGroupEntry {
                    binding: 9,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [matrix.shape[1] as u32 / 4, shape[1] as u32, shape[2] as u32],
        })
    }

    /// Add `input` onto `output`.
    pub fn add(
        input: &'a TensorGpu<f32, ReadWrite>,
        output: &'a TensorGpu<f32, ReadWrite>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape;
        input.check_shape(shape)?;

        let context = output.context;
        let pipeline = context.pipeline("add")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: input.binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }

    pub fn token_shift(
        time_mix: &'a TensorGpu<f16, ReadWrite>,
        x: &'a TensorGpu<f32, ReadWrite>,
        sx: TensorView<'a, f32>,
        output: &'a TensorGpu<f32, ReadWrite>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape;
        time_mix.check_shape(Shape::new(shape[0], 1, 1))?;
        x.check_shape(shape)?;
        sx.check_shape(Shape::new(shape[0], 1, shape[2]))
            .or(sx.check_shape(Shape::new(shape[0], 4, shape[2])))?;

        let context = output.context;
        let pipeline = context.pipeline("token_shift")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: sx.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: time_mix.binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: x.binding(),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: sx.binding(),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn token_mix(
        mask: &'a TensorGpu<u32, Uniform>,
        time_decay: &'a TensorGpu<f32, ReadWrite>,
        time_first: &'a TensorGpu<f32, ReadWrite>,
        x: &'a TensorGpu<f32, ReadWrite>,
        k: &'a TensorGpu<f32, ReadWrite>,
        v: &'a TensorGpu<f32, ReadWrite>,
        r: &'a TensorGpu<f32, ReadWrite>,
        output: &'a TensorGpu<f32, ReadWrite>,
        state: TensorView<f32>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape;
        mask.check_shape(Shape::new(1, 1, 1))?;
        x.check_shape(shape)?;
        k.check_shape(shape)?;
        v.check_shape(shape)?;
        r.check_shape(shape)?;
        time_decay.check_shape(Shape::new(shape[0], 1, 1))?;
        time_first.check_shape(Shape::new(shape[0], 1, 1))?;
        state.check_shape(Shape::new(shape[0], 4, shape[2]))?;

        let context = output.context;
        let pipeline = context.pipeline("token_mix")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: state.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: mask.binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: time_decay.binding(),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: time_first.binding(),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: x.binding(),
                },
                BindGroupEntry {
                    binding: 6,
                    resource: k.binding(),
                },
                BindGroupEntry {
                    binding: 7,
                    resource: v.binding(),
                },
                BindGroupEntry {
                    binding: 8,
                    resource: r.binding(),
                },
                BindGroupEntry {
                    binding: 9,
                    resource: output.binding(),
                },
                BindGroupEntry {
                    binding: 10,
                    resource: state.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [Self::block_count(shape[0] as u32 / 4), 1, shape[2] as u32],
        })
    }

    pub fn squared_relu(x: &'a TensorGpu<f32, ReadWrite>) -> Result<Self, TensorError> {
        let shape = x.shape;
        let context = x.context;
        let pipeline = context.pipeline("squared_relu")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: x.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: x.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }

    pub fn channel_mix(
        mask: &'a TensorGpu<u32, Uniform>,
        x: &'a TensorGpu<f32, ReadWrite>,
        r: &'a TensorGpu<f32, ReadWrite>,
        v: &'a TensorGpu<f32, ReadWrite>,
        output: &'a TensorGpu<f32, ReadWrite>,
        state: TensorView<f32>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape;
        mask.check_shape(Shape::new(1, 1, 1))?;
        x.check_shape(shape)?;
        v.check_shape(shape)?;
        r.check_shape(shape)?;
        state.check_shape(Shape::new(shape[0], 1, shape[2]))?;

        let context = output.context;
        let pipeline = context.pipeline("channel_mix")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: state.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: mask.binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: x.binding(),
                },
                BindGroupEntry {
                    binding: 4,
                    resource: r.binding(),
                },
                BindGroupEntry {
                    binding: 5,
                    resource: v.binding(),
                },
                BindGroupEntry {
                    binding: 6,
                    resource: output.binding(),
                },
                BindGroupEntry {
                    binding: 7,
                    resource: state.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }

    /// Copy the content of `input` into `output`, given an `offset`.
    pub fn blit(
        input: TensorView<'a, f32>,
        output: TensorView<'a, f32>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape();
        input.check_shape(shape)?;

        let context = input.context;
        let pipeline = context.pipeline("blit")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: input.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: input.binding(),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }

    pub fn quantize_mat_int8(
        input: &'a TensorGpu<f16, ReadWrite>,
        mx: &'a TensorGpu<f32, ReadWrite>,
        rx: &'a TensorGpu<f32, ReadWrite>,
        my: &'a TensorGpu<f32, ReadWrite>,
        ry: &'a TensorGpu<f32, ReadWrite>,
        output: &'a TensorGpu<u8, ReadWrite>,
    ) -> Result<Vec<Self>, TensorError> {
        let shape = output.shape;
        input.check_shape(shape)?;
        mx.check_shape(Shape::new(shape[0], 1, 1))?;
        rx.check_shape(Shape::new(shape[0], 1, 1))?;
        my.check_shape(Shape::new(shape[1], 1, 1))?;
        ry.check_shape(Shape::new(shape[1], 1, 1))?;

        let context = output.context;
        let entries = &[
            BindGroupEntry {
                binding: 0,
                resource: output.meta_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: input.binding(),
            },
            BindGroupEntry {
                binding: 2,
                resource: mx.binding(),
            },
            BindGroupEntry {
                binding: 3,
                resource: rx.binding(),
            },
            BindGroupEntry {
                binding: 4,
                resource: my.binding(),
            },
            BindGroupEntry {
                binding: 5,
                resource: ry.binding(),
            },
            BindGroupEntry {
                binding: 6,
                resource: output.binding(),
            },
        ];
        let create_op = |name: &'static str, dispatch| -> Result<Self, TensorError> {
            let pipeline = context.pipeline(name)?;
            let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
                label: None,
                layout: &pipeline.get_bind_group_layout(0),
                entries,
            })];
            Ok(Self {
                pipeline,
                bindings,
                dispatch,
            })
        };

        let my = create_op("quant_mat_int8_my", [1, shape[1] as u32, 1])?;
        let ry = create_op("quant_mat_int8_ry", [1, shape[1] as u32, 1])?;
        let mx = create_op("quant_mat_int8_mx", [1, shape[0] as u32 / 4, 1])?;
        let rx = create_op("quant_mat_int8_rx", [1, shape[0] as u32 / 4, 1])?;
        let quantize = create_op("quant_mat_int8", [shape[0] as u32 / 4, shape[1] as u32, 1])?;

        if shape[1] > shape[0] {
            Ok(vec![my, mx, rx, ry, quantize])
        } else {
            Ok(vec![mx, my, rx, ry, quantize])
        }
    }

    pub fn quantize_vec_fp16(
        input: &'a TensorGpu<f32, ReadWrite>,
        output: &'a TensorGpu<f16, ReadWrite>,
    ) -> Result<Self, TensorError> {
        let shape = output.shape;
        input.check_shape(shape)?;

        let context = output.context;
        let pipeline = context.pipeline("quant_vec_fp16")?;
        let bindings = vec![context.device.create_bind_group(&BindGroupDescriptor {
            label: None,
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: output.meta_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: input.binding(),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: output.binding(),
                },
            ],
        })];

        Ok(Self {
            pipeline,
            bindings,
            dispatch: [
                Self::block_count(shape[0] as u32 / 4),
                shape[1] as u32,
                shape[2] as u32,
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use half::f16;
    use itertools::Itertools;
    use wgpu::{CommandEncoderDescriptor, ComputePassDescriptor, PowerPreference};

    use super::{TensorOp, TensorPass};
    use crate::{
        context::{Context, ContextBuilder, Instance},
        tensor::{ops::TensorCommand, Shape, TensorCpu, TensorExt, TensorGpu, TensorView},
    };

    fn is_approx(a: f32, b: f32) -> bool {
        (a - b).abs() <= f32::max(f32::EPSILON, f32::max(a.abs(), b.abs()) * f32::EPSILON)
    }

    fn is_approx_eps(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= f32::max(eps, f32::max(a.abs(), b.abs()) * eps)
    }

    fn create_context() -> Result<Context, anyhow::Error> {
        let adapter = pollster::block_on(async {
            let instance = Instance::new();
            instance.adapter(PowerPreference::HighPerformance).await
        })?;
        let context = pollster::block_on(async {
            ContextBuilder::new(adapter)
                .with_default_pipelines()
                .build()
                .await
        })?;
        Ok(context)
    }

    #[test]
    fn test_copy() -> Result<(), anyhow::Error> {
        let context = match create_context() {
            Ok(context) => context,
            Err(_) => return Ok(()),
        };

        let x = vec![0.0, 1.5, 2.0, -1.0];
        let shape = Shape::new(x.len(), 1, 1);

        let x_device: TensorGpu<_, _> = context.tensor_from_data(shape, x.clone())?;
        let x_map = context.init_tensor(x_device.shape());

        let mut encoder = context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_tensor(&x_device, &x_map)?;
        context.queue.submit(Some(encoder.finish()));

        let x_host = TensorCpu::from(x_map);
        let x_host = Vec::from(x_host);

        assert_eq!(x, x_host);
        Ok(())
    }

    #[test]
    fn test_softmax() -> Result<(), anyhow::Error> {
        let context = match create_context() {
            Ok(context) => context,
            Err(_) => return Ok(()),
        };

        const C: usize = 1000;
        const T: usize = 3;
        const B: usize = 2;

        let x = [(); C * T * B]
            .map(|_| 10.0 * (fastrand::f32() - 0.5))
            .to_vec();
        let shape = Shape::new(C, T, B);

        let x_dev: TensorGpu<_, _> = context.tensor_from_data(shape, x.clone())?;
        let x_map = context.init_tensor(x_dev.shape());

        let softmax = TensorOp::softmax(&x_dev)?;

        let mut encoder = context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
        pass.execute_tensor_op(&softmax);
        drop(pass);

        encoder.copy_tensor(&x_dev, &x_map)?;
        context.queue.submit(Some(encoder.finish()));

        let x_host = TensorCpu::from(x_map);
        let x_host = Vec::from(x_host);

        let mut ans = vec![];
        for x in &x.into_iter().chunks(C) {
            let x: Vec<_> = x.collect();
            let x = x.into_iter();
            let max = x.clone().reduce(f32::max).unwrap_or_default();
            let x = x.map(|x| (x - max).exp());
            let sum: f32 = x.clone().sum();
            let mut x: Vec<_> = x.map(|x| x / sum).collect();
            ans.append(&mut x);
        }

        for (index, (a, b)) in Iterator::zip(x_host.into_iter(), ans.into_iter()).enumerate() {
            assert!(
                is_approx(a, b),
                "Failed at index {index}, computed: {a} vs. answer: {b}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_layer_norm() -> Result<(), anyhow::Error> {
        let context = match create_context() {
            Ok(context) => context,
            Err(_) => return Ok(()),
        };

        const C: usize = 1000;
        const T: usize = 3;
        const B: usize = 2;

        let x = [(); C * T * B]
            .map(|_| 10.0 * (fastrand::f32() - 0.5))
            .to_vec();
        let w = [(); C]
            .map(|_| f16::from_f32(fastrand::f32() - 0.5))
            .repeat(T * B)
            .to_vec();
        let b = [(); C]
            .map(|_| f16::from_f32(fastrand::f32() - 0.5))
            .repeat(T * B)
            .to_vec();

        let shape = Shape::new(C, T, B);
        let x_dev = TensorGpu::from_data(&context, shape, &x)?;
        let x_map = context.init_tensor(shape);

        let shape = Shape::new(C, 1, 1);
        let w_dev = TensorGpu::from_data(&context, shape, &w[..1000])?;
        let b_dev = TensorGpu::from_data(&context, shape, &b[..1000])?;

        let layer_norm = TensorOp::layer_norm(&w_dev, &b_dev, &x_dev)?;

        let mut encoder = context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
        pass.execute_tensor_op(&layer_norm);
        drop(pass);

        encoder.copy_tensor(&x_dev, &x_map)?;
        context.queue.submit(Some(encoder.finish()));

        let x_host = TensorCpu::from(x_map);
        let x_host = Vec::from(x_host);

        let mut ans = vec![];
        for chunk in &x
            .into_iter()
            .zip(w.into_iter())
            .zip(b.into_iter())
            .chunks(C)
        {
            let chunk: Vec<_> = chunk.collect();
            let x = chunk.iter().map(|((x, _), _)| x).copied();
            let sum: f32 = x.clone().sum();
            let squared_sum: f32 = x.clone().map(|x| x.powi(2)).sum();

            let mean = sum / C as f32;
            let deviation = ((squared_sum / C as f32) - mean.powi(2)).sqrt();

            let mut x: Vec<_> = chunk
                .into_iter()
                .map(|((x, w), b)| (x - mean) / deviation * w.to_f32() + b.to_f32())
                .collect();
            ans.append(&mut x);
        }

        for (index, (a, b)) in Iterator::zip(x_host.into_iter(), ans.into_iter()).enumerate() {
            assert!(
                is_approx_eps(a, b, 1.0e-3),
                "Failed at index {index}, computed: {a} vs. answer: {b}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_matmul() -> Result<(), anyhow::Error> {
        let context = match create_context() {
            Ok(context) => context,
            Err(_) => return Ok(()),
        };

        const C: usize = 1024;
        const R: usize = 768;
        const T: usize = 7;
        const B: usize = 3;

        let matrix: Vec<_> = vec![(); C * R]
            .into_iter()
            .map(|_| 10.0 * (fastrand::f32() - 0.5))
            .map(f16::from_f32)
            .collect();
        let input: Vec<_> = vec![(); C * T * B]
            .into_iter()
            .map(|_| 10.0 * (fastrand::f32() - 0.5))
            .collect();

        let matrix_dev = TensorGpu::from_data(&context, Shape::new(C, R, 1), matrix.clone())?;
        let input_dev = TensorGpu::from_data(&context, Shape::new(C, T, B), input.clone())?;
        let output_dev = TensorGpu::init(&context, Shape::new(R * 2, T, B));
        let output_map = TensorGpu::init(&context, output_dev.shape());

        let matmul = TensorOp::matmul(
            &matrix_dev,
            input_dev.clone().into(),
            output_dev.as_view((R.., .., ..))?,
        )?;

        let mut encoder = context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
        pass.execute_tensor_op(&matmul);
        drop(pass);

        encoder.copy_tensor(&output_dev, &output_map)?;
        context.queue.submit(Some(encoder.finish()));

        let output_host = TensorCpu::from(output_map);
        let output_host = Vec::from(output_host);

        let mut ans = vec![0.0; output_host.len()];
        for batch in 0..B {
            for token in 0..T {
                for line in 0..R {
                    let matrix = &matrix[line * C..(line + 1) * C];
                    let input = &input[(batch * T + token) * C..(batch * T + token + 1) * C];
                    let product = matrix
                        .iter()
                        .map(|x| (*x).to_f32())
                        .zip(input.iter())
                        .fold(0.0f32, |acc, x| acc + x.0 * *x.1);
                    ans[(batch * T + token) * 2 * R + R + line] = product;
                }
            }
        }

        for (index, (a, b)) in Iterator::zip(output_host.into_iter(), ans.into_iter()).enumerate() {
            assert!(
                is_approx_eps(a, b, 1.0e-3),
                "Failed at index {index}, computed: {a} vs. answer: {b}"
            );
        }

        Ok(())
    }

    #[test]
    fn test_blit() -> Result<(), anyhow::Error> {
        let context = match create_context() {
            Ok(context) => context,
            Err(_) => return Ok(()),
        };

        let output = vec![0.0; 24];
        let output = TensorGpu::from_data(&context, Shape::new(4, 3, 2), output)?;

        let map = TensorGpu::init(&context, output.shape());
        let mut ops = vec![];

        let input: Vec<_> = (0..8).map(|x| x as f32).collect();
        let input = TensorGpu::from_data(&context, Shape::new(4, 1, 2), input)?;
        ops.push(TensorOp::blit(
            input.as_view((.., .., ..))?,
            output.as_view((.., 1, ..))?,
        )?);

        let input: Vec<_> = (8..12).map(|x| x as f32).collect();
        let input = TensorView::from_data(&context, Shape::new(4, 1, 1), input)?;
        ops.push(TensorOp::blit(input, output.as_view((.., 2.., 1..2))?)?);

        let mut encoder = context
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        let mut pass = encoder.begin_compute_pass(&ComputePassDescriptor::default());
        ops.iter().for_each(|op| pass.execute_tensor_op(op));
        drop(pass);

        encoder.copy_tensor(&output, &map)?;
        context.queue.submit(Some(encoder.finish()));

        let output_host = TensorCpu::from(map);
        let output_host = Vec::from(output_host);

        assert_eq!(
            output_host,
            vec![
                0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
                4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0
            ]
        );

        Ok(())
    }
}