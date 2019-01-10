use ndarray::*;
use ops::prelude::*;

fn eval_t<T: Datum + LinalgScalar>(a: &Tensor, b: &Tensor) -> TractResult<Tensor> {
    let a = a.to_array_view::<T>()?;
    let b = b.to_array_view::<T>()?;
    let geo = Geo::<T>::new(a.shape(), b.shape())?;
    let (ashape, bshape, cshape) = infer_shapes(a.shape().into(), b.shape().into())?;
    let a = a.into_shape(&*ashape)?;
    let b = b.into_shape(&*bshape)?;
    let mut c = unsafe { Array::uninitialized(&*cshape) };

    let mut pa = Vec::with_capacity(geo.mm.packed_a_len());
    let mut pb = Vec::with_capacity(geo.mm.packed_b_len());

    for prefix in indices(&*geo.c_shape_prefix).into_iter() {
        let mut a = a.view();
        let mut b = b.view();
        let mut c = c.view_mut();
        for (axis, &dim) in prefix.slice().iter().enumerate() {
            a.slice_axis_inplace(Axis(axis), (dim..=dim).into());
            b.slice_axis_inplace(Axis(axis), (dim..=dim).into());
            c.slice_axis_inplace(Axis(axis), (dim..=dim).into());
        }

        geo.mm.pack_a(
            pa.as_mut_ptr(),
            a.as_ptr(),
            a.strides()[prefix.ndim()],
            a.strides()[prefix.ndim() + 1],
        );
        geo.mm.pack_b(
            pb.as_mut_ptr(),
            b.as_ptr(),
            b.strides()[prefix.ndim()],
            b.strides()[prefix.ndim() + 1],
        );
        geo.mm.mat_mul_prepacked(
            pa.as_ptr(),
            pb.as_ptr(),
            c.as_mut_ptr(),
            c.strides()[prefix.ndim()],
            c.strides()[prefix.ndim() + 1],
        );
    }
    Ok(c.into())
}

fn infer_shapes<D: DimLike>(
    mut ashape: TVec<D>,
    mut bshape: TVec<D>,
) -> TractResult<(TVec<D>, TVec<D>, TVec<D>)> {
    if ashape.len() < 2 {
        ashape.insert(0, D::one());
    }
    if bshape.len() < 2 {
        bshape.push(D::one());
    }
    while ashape.len() < bshape.len() {
        ashape.insert(0, D::one());
    }
    while bshape.len() < ashape.len() {
        bshape.insert(0, D::one());
    }
    let cshape_prefix = ::broadcast::multi_broadcast(&[
        &ashape[..(ashape.len() - 2)],
        &bshape[..(bshape.len() - 2)],
    ])
    .ok_or("Could not broadcast")?;
    let mut cshape: TVec<D> = cshape_prefix.clone();
    cshape.push(ashape[ashape.len() - 2]);
    cshape.push(bshape[bshape.len() - 1]);
    Ok((ashape, bshape, cshape))
}

#[derive(Debug)]
struct Geo<T> {
    m: usize,
    k: usize,
    n: usize,
    mm: Box<tract_linalg::MatMul<T>>,
    c_shape_prefix: TVec<usize>,
    a_stride_prefix: TVec<usize>,
    b_stride_prefix: TVec<usize>,
    c_stride_prefix: TVec<usize>,
}

impl<T: Datum> Geo<T> {
    pub fn new(a_shape: &[usize], b_shape: &[usize]) -> TractResult<Geo<T>> {
        let (bc_a_shape, bc_b_shape, bc_c_shape) = infer_shapes(a_shape.into(), b_shape.into())?;
        let m = bc_a_shape[bc_a_shape.len() - 2];
        let k = bc_a_shape[bc_a_shape.len() - 1];
        let n = bc_b_shape[bc_b_shape.len() - 1];
        let mm = T::packed_mat_mul(m, k, n).ok_or_else(|| {
            format!(
                "Can not perfom matmul on {:?} (not a linear algebra type)",
                T::datum_type()
            )
        })?;
        let a_stride_prefix = bc_a_shape
            .iter()
            .rev()
            .scan(1, |stride, dim| {
                let s = Some(*stride);
                *stride *= dim;
                s
            })
            .skip(2)
            .collect();
        let b_stride_prefix = bc_b_shape
            .iter()
            .rev()
            .scan(1, |stride, dim| {
                let s = Some(*stride);
                *stride *= dim;
                s
            })
            .skip(2)
            .collect();
        let c_stride_prefix = bc_c_shape
            .iter()
            .rev()
            .scan(1, |stride, dim| {
                let s = Some(*stride);
                *stride *= dim;
                s
            })
            .skip(2)
            .collect();
        Ok(Geo {
            m,
            k,
            n,
            mm,
            c_shape_prefix: bc_c_shape[0..(bc_c_shape.len() - 2)].into(),
            a_stride_prefix,
            b_stride_prefix,
            c_stride_prefix,
        })
    }
}

#[derive(Debug, Clone, new, Default)]
pub struct MatMul {}

impl Op for MatMul {
    fn name(&self) -> Cow<str> {
        "MatMul".into()
    }
}

impl StatelessOp for MatMul {
    fn eval(&self, mut inputs: TVec<SharedTensor>) -> TractResult<TVec<SharedTensor>> {
        let (a, b) = args_2!(inputs);
        let c = dispatch_floatlike!(self::eval_t(a.datum_type())(a.as_tensor(), b.as_tensor()))?;
        Ok(tvec!(c.into()))
    }
}

impl InferenceRulesOp for MatMul {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p SharedTensorsProxy,
        outputs: &'p SharedTensorsProxy,
    ) -> InferenceResult {
        s.equals(&inputs.len, 2)?;
        s.equals(&outputs.len, 1)?;
        s.equals(&inputs[0].datum_type, &outputs[0].datum_type)?;
        s.equals(&inputs[1].datum_type, &outputs[0].datum_type)?;
        s.given_2(
            &inputs[0].shape,
            &inputs[1].shape,
            move |s, ashape, bshape| {
                let (_, _, cshape) = infer_shapes(ashape, bshape)?;
                s.equals(&outputs[0].shape, cshape)
            },
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, new)]
pub struct MatMulUnaryA {
    b: Tensor,
}

impl Op for MatMulUnaryA {
    fn name(&self) -> Cow<str> {
        "MatMulUnaryA".into()
    }

    fn pulsify(&self, mut inputs: TVec<&PulsedTensorFact>) -> TractResult<Vec<PulsifiedOp>> {
        let input = args_1!(inputs);
        if input.axis >= input.shape.len() - 1 {
            bail!("Can not pulsify MatMulUnaryA on the most inner dimension (k)");
        }
        let (_, _, cshape_pulse) = infer_shapes(input.shape.clone(), self.b.shape().into())?;
        let (_, _, cshape_full) = infer_shapes(
            input.streaming_shape().into(),
            self.b.shape().iter().map(|d| d.to_dim()).collect(),
        )?;
        let mut fact = input.clone();
        fact.shape = cshape_pulse;
        fact.dim = cshape_full[fact.axis];
        Ok(vec![PulsifiedOp::new(Box::new(self.clone()), tvec!(fact))])
    }
}

impl StatelessOp for MatMulUnaryA {
    fn eval(&self, mut inputs: TVec<SharedTensor>) -> TractResult<TVec<SharedTensor>> {
        let a = args_1!(inputs);
        let c = dispatch_floatlike!(self::eval_t(a.datum_type())(a.as_tensor(), &self.b))?;
        Ok(tvec!(c.into()))
    }
}

impl InferenceRulesOp for MatMulUnaryA {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p SharedTensorsProxy,
        outputs: &'p SharedTensorsProxy,
    ) -> InferenceResult {
        s.equals(&inputs.len, 1)?;
        s.equals(&outputs.len, 1)?;
        s.equals(&inputs[0].datum_type, &outputs[0].datum_type)?;
        s.given(&inputs[0].shape, move |s, ashape| {
            let bshape: TVec<TDim> = self.b.shape().iter().map(|x| x.to_dim()).collect();
            let (_, _, cshape) = infer_shapes(ashape, bshape)?;
            s.equals(&outputs[0].shape, cshape)
        })?;
        Ok(())
    }
}

#[derive(Debug, Clone, new)]
pub struct MatMulUnaryB {
    a: Tensor,
}

impl Op for MatMulUnaryB {
    fn name(&self) -> Cow<str> {
        "MatMulUnaryB".into()
    }
}

impl StatelessOp for MatMulUnaryB {
    fn eval(&self, mut inputs: TVec<SharedTensor>) -> TractResult<TVec<SharedTensor>> {
        let b = args_1!(inputs);
        let c = dispatch_floatlike!(self::eval_t(b.datum_type())(&self.a, b.as_tensor()))?;
        Ok(tvec!(c.into()))
    }
}

impl InferenceRulesOp for MatMulUnaryB {
    fn rules<'r, 'p: 'r, 's: 'r>(
        &'s self,
        s: &mut Solver<'r>,
        inputs: &'p SharedTensorsProxy,
        outputs: &'p SharedTensorsProxy,
    ) -> InferenceResult {
        s.equals(&inputs.len, 1)?;
        s.equals(&outputs.len, 1)?;
        s.equals(&inputs[0].datum_type, &outputs[0].datum_type)?;
        s.given(&inputs[0].shape, move |s, bshape| {
            let ashape: TVec<TDim> = self.a.shape().iter().map(|x| x.to_dim()).collect();
            let (_, _, cshape) = infer_shapes(ashape, bshape)?;
            s.equals(&outputs[0].shape, cshape)
        })?;
        Ok(())
    }
}
