use super::*;
use crate::ops::cast::cast;
use crate::ops::math::add;
use crate::ops::matmul::lir_unary::{
    AddMatMulGeometry, LirMatMulUnary, MapOutputAxisToInput, ProtoFusedSpec,
};
use crate::ops::matmul::mir_quant::{
    combine_scales, compensate_zero_points, requant, wire_offset_u8_as_i8,
};
use crate::ops::matmul::pack::MatMatMulPack;
use crate::ops::nn::{Reduce, Reducer};

pub enum AxesOrPatch<'a> {
    Axes(&'a Axis, &'a Axis, &'a Axis),
    Patch(TypedModelPatch),
}

pub(crate) fn codegen(
    op: &EinSum,
    model: &TypedModel,
    node: &TypedNode,
) -> TractResult<Option<TypedModelPatch>> {
    if (op.q_params.is_none() && node.inputs.len() != 2)
        || (op.q_params.is_some() && node.inputs.len() != 9)
    {
        return Ok(None);
    }
    let (m_axis, k_axis, n_axis) = match ensure_mkn_axes(op, model, node)? {
        AxesOrPatch::Axes(m, k, n) => (m, k, n),
        AxesOrPatch::Patch(p) => return Ok(Some(p)),
    };
    if op.q_params.is_none() {
        lir_mat_mul_unary(op, model, node, (m_axis, k_axis, n_axis))
            .context("Translating to LirMatMul")
    } else {
        dequant_output(op, model, node, (m_axis, k_axis, n_axis)).context("Dequantizing output")
    }
}

pub(super) fn ensure_mkn_axes<'a>(
    op: &'a EinSum,
    model: &TypedModel,
    node: &TypedNode,
) -> TractResult<AxesOrPatch<'a>> {
    let input_facts = model.node_input_facts(node.id)?;
    let input_shapes: TVec<&[TDim]> = input_facts.iter().map(|f| &*f.shape).collect();
    let output_shape = super::eval::output_shape(&op.axes, &input_shapes);
    let candidate_k_axes: TVec<&Axis> = op
        .axes
        .iter_all_axes()
        // Filter possible candidates (should be one time in each inputs but not in output)
        .filter(|a| {
            a.inputs[0].len() == 1 && a.inputs[1].len() == 1 && a.outputs[0].len() == 0 &&
                input_facts[0].shape[a.inputs[0][0]] == input_facts[1].shape[a.inputs[1][0]]
        })
    .collect();

    let non_trivial_k_axis = candidate_k_axes
        .iter()
        .filter(|a| input_facts[0].shape[a.inputs[0][0]] > 1.to_dim())
        .collect::<TVec<_>>();

    let k_axis = if non_trivial_k_axis.len() > 1 {
        // TODO: handle case where multiple consecutive k in the same order in both input.
        bail!("Multiple k-axis candidate found");
    } else {
        non_trivial_k_axis.get(0).copied().or_else(|| candidate_k_axes.get(0)).copied()
    };
    let Some(k_axis) = k_axis else {
        return Ok(AxesOrPatch::Patch(inject_k_axis(op, model, node)?));
    };
    let m_axis = op
        .axes
        .iter_all_axes()
        .filter(|a| {
            a.inputs[0].len() == 1
                && (a.inputs[1].len() == 0 || input_facts[1].shape[a.inputs[1][0]].is_one())
                && a.outputs[0].len() == 1
        })
        .max_by_key(|a| &output_shape[a.outputs[0][0]]);
    let Some(m_axis) = m_axis else {
        return Ok(AxesOrPatch::Patch(inject_m_or_n_axis(op, model, node, false, &[k_axis])?));
    };
    let n_axis = op
        .axes
        .iter_all_axes()
        .filter(|a| {
            (a.inputs[0].len() == 0 || input_facts[0].shape[a.inputs[0][0]].is_one())
                && a.inputs[1].len() == 1
                && a.outputs[0].len() == 1
        })
        .max_by_key(|a| &output_shape[a.outputs[0][0]]);
    let Some(n_axis) = n_axis else {
        return Ok(AxesOrPatch::Patch(inject_m_or_n_axis(op, model, node, true, &[k_axis, m_axis])?));
    };
    Ok(AxesOrPatch::Axes(m_axis, k_axis, n_axis))
}

pub(super) fn inject_k_axis(
    op: &EinSum,
    model: &TypedModel,
    node: &TypedNode,
) -> TractResult<TypedModelPatch> {
    let mut new_axes = op.axes.clone();
    let name = &node.name;
    let mut patch = TypedModelPatch::new("inject k axis");
    let mut wire =
        node.inputs.iter().map(|i| patch.tap_model(model, *i)).collect::<TractResult<TVec<_>>>()?;
    let possible_k_axis =
        new_axes.iter_all_axes().find(|a| a.outputs[0].len() == 0).map(|axis| axis.repr);
    if let Some(axis) = possible_k_axis {
        let input_to_fix = (new_axes.axis(axis)?.inputs[0].len() > 0) as usize;
        new_axes = new_axes.with_extra_axis_occurency(axis, InOut::In(input_to_fix), 0)?;
        wire[input_to_fix] =
            patch.wire_node(format!("{name}.add_k"), AxisOp::Add(0), &[wire[input_to_fix]])?[0];
    } else {
        let repr = new_axes.available_label();
        new_axes = new_axes.with_extra_axis(repr, InOut::In(0), 0)?.with_extra_axis_occurency(
            repr,
            InOut::In(1),
            0,
        )?;
        wire[0] = patch.wire_node(format!("{name}.add_k.0"), AxisOp::Add(0), &[wire[0]])?[0];
        wire[1] = patch.wire_node(format!("{name}.add_k.1"), AxisOp::Add(0), &[wire[1]])?[0];
    };
    wire = patch.wire_node(&node.name, EinSum { axes: new_axes, ..op.clone() }, &wire)?;
    patch.shunt_outside(model, node.id.into(), wire[0])?;
    Ok(patch)
}

pub(super) fn inject_m_or_n_axis(
    op: &EinSum,
    model: &TypedModel,
    node: &TypedNode,
    is_n: bool,
    exclude: &[&Axis],
) -> TractResult<TypedModelPatch> {
    let input_to_fix = is_n as usize;
    let label = if is_n { "n" } else { "m" };
    let input_facts = model.node_input_facts(node.id)?;
    let quasi_m_or_n_axis = op.axes.iter_all_axes().filter(|a| !exclude.contains(a)).find(|a| {
        (a.inputs[1 - input_to_fix].len() == 0
            || input_facts[1 - input_to_fix].shape[a.inputs[1 - input_to_fix][0]].is_one())
            && (a.inputs[input_to_fix].len() == 1 || a.outputs[0].len() == 1)
    });
    let name = &node.name;
    let mut patch = TypedModelPatch::new("Injecting m or n axis");
    let mut wire =
        node.inputs.iter().map(|i| patch.tap_model(model, *i)).collect::<TractResult<TVec<_>>>()?;
    if let Some(axis) = quasi_m_or_n_axis {
        if axis.inputs[input_to_fix].len() == 1 {
            let new_axes =
                op.axes.clone().with_extra_axis('$', InOut::Out(0), 0)?.linking(axis.repr, '$')?;
            wire = patch.wire_node(
                format!("{name}.einsum"),
                EinSum { axes: new_axes, ..op.clone() },
                &wire,
            )?;
            wire = patch.wire_node(&node.name, AxisOp::Rm(0), &wire)?;
        } else {
            let new_axes = op
                .axes
                .clone()
                .with_extra_axis('$', InOut::In(input_to_fix), 0)?
                .linking(axis.repr, '$')?;
            wire[input_to_fix] =
                patch.wire_node(format!("{name}.add_{label}"), AxisOp::Add(0), &[wire[input_to_fix]])?
                    [0];
            wire = patch.wire_node(&node.name, EinSum { axes: new_axes, ..op.clone() }, &wire)?;
        }
    } else {
        let repr = op.axes.available_label();
        let new_axes = op
            .axes
            .clone()
            .with_extra_axis(repr, InOut::In(input_to_fix), 0)?
            .with_extra_axis('$', InOut::Out(0), 0)?
            .linking(repr, '$')?;
        wire[input_to_fix] =
            patch.wire_node(format!("{name}.add_{label}"), AxisOp::Add(0), &[wire[input_to_fix]])?[0];
        wire = patch.wire_node(
            format!("{name}.einsum"),
            EinSum { axes: new_axes, ..op.clone() },
            &wire,
        )?;
        wire = patch.wire_node(&node.name, AxisOp::Rm(0), &wire)?;
    }
    patch.shunt_outside(model, node.id.into(), wire[0])?;
    Ok(patch)
}

fn wire_axes_fix(
    patch: &mut TypedModelPatch,
    name: &str,
    var: &str,
    mapping: &AxesMapping,
    mut outlet: TVec<OutletId>,
) -> TractResult<TVec<OutletId>> {
    for (ix, axis_op) in mapping.translate_to_axis_ops()?.into_iter().enumerate() {
        outlet = patch.wire_node(format!("{name}.fix_{var}.{ix})"), axis_op, &outlet)?;
    }
    Ok(outlet)
}

fn dequant_output(
    op: &EinSum,
    model: &TypedModel,
    node: &TypedNode,
    (_, k_axis, _): (&Axis, &Axis, &Axis),
) -> TractResult<Option<TypedModelPatch>> {
    let name = &node.name;
    let mut patch = TypedModelPatch::new("Dequantizing einsum");
    let taps: Vec<OutletId> =
        node.inputs.iter().map(|i| patch.tap_model(model, *i)).collect::<TractResult<Vec<_>>>()?;
    let [a, b, bias, mut a0, a_scale, mut b0, b_scale, c0, c_scale] = *taps else {
        bail!("Expect exactly 9 inputs")
    };

    let a = wire_offset_u8_as_i8(&mut patch, &node.name, a, "a", &mut a0, "a0")?;
    let b = wire_offset_u8_as_i8(&mut patch, &node.name, b, "b", &mut b0, "b0")?;

    let mut output = patch.wire_node(
        &node.name,
        EinSum {
            q_params: None,
            axes: op.axes.extract_sub_mapping(&[0, 1], &[0])?,
            operating_dt: op.operating_dt,
        },
        &[a, b],
    )?;

    let a_i32 = patch.wire_node(format!("{name}.a_as_i32"), cast(i32::datum_type()), &[a])?[0];
    let b_i32 = patch.wire_node(format!("{name}.b_as_i32"), cast(i32::datum_type()), &[b])?[0];
    let sum_a = patch.wire_node(
        format!("{name}.sum_a"),
        Reduce::new(tvec!(k_axis.inputs[0][0]), Reducer::Sum),
        &[a_i32],
    )?;
    let sum_b = patch.wire_node(
        format!("{name}.sum_b"),
        Reduce::new(tvec!(k_axis.inputs[1][0]), Reducer::Sum),
        &[b_i32],
    )?;

    let sum_a =
        wire_axes_fix(&mut patch, name, "sum_a", &op.axes.extract_sub_mapping(&[0], &[0])?, sum_a)?;
    let sum_b =
        wire_axes_fix(&mut patch, name, "sum_b", &op.axes.extract_sub_mapping(&[1], &[0])?, sum_b)?;
    let bias = tvec!(bias);
    let bias =
        wire_axes_fix(&mut patch, name, "bias", &op.axes.extract_sub_mapping(&[2], &[0])?, bias)?;

    let abc_scale = combine_scales(&mut patch, name, a_scale, b_scale, c_scale)?;

    output = patch.wire_node(format!("{name}.add_bias"), add(), &[output[0], bias[0]])?;

    let k = model.outlet_fact(node.inputs[0])?.shape[k_axis.inputs[0][0]].clone();
    let output = compensate_zero_points(&mut patch, name, output[0], k, a0, b0, sum_a[0], sum_b[0])
        .context("Zero point compensation")?;
    let output = requant(&mut patch, name, output, op.q_params.unwrap(), abc_scale, c0)?;
    patch.shunt_outside(model, node.id.into(), output)?;
    Ok(Some(patch))
}

fn lir_mat_mul_unary(
    op: &EinSum,
    model: &TypedModel,
    node: &TypedNode,
    (m_axis, k_axis, n_axis): (&Axis, &Axis, &Axis),
) -> TractResult<Option<TypedModelPatch>> {
    let input_facts = model.node_input_facts(node.id)?;
    let a_m = m_axis.inputs[0][0];
    let a_k = k_axis.inputs[0][0];
    let b_n = n_axis.inputs[1][0];
    let b_k = k_axis.inputs[1][0];
    let c_m = m_axis.outputs[0][0];
    let c_n = n_axis.outputs[0][0];
    let m = &input_facts[0].shape[a_m];
    let k = &input_facts[0].shape[a_k];
    let n = &input_facts[1].shape[b_n];
    if m < n {
        let expr = op
            .axes
            .iter_all_axes()
            .map(|axis| {
                let mut axis = axis.clone();
                axis.inputs.swap(0, 1);
                axis
            })
            .collect::<TVec<Axis>>();
        return TypedModelPatch::replace_single_op(
            model,
            node,
            &[node.inputs[1], node.inputs[0]],
            EinSum { axes: AxesMapping::new(node.inputs.len(), 1, expr)?, ..op.clone() },
        )
        .map(Some);
    }
    let a_dt = input_facts[0].datum_type;
    let b_dt = input_facts[1].datum_type;
    let dt = op.operating_dt;
    let mmm = tract_linalg::ops()
        .mmm(a_dt, b_dt, dt, m.to_usize().ok(), k.to_usize().ok(), n.to_usize().ok())
        .unwrap();
    let name = &node.name;
    let mut patch = TypedModelPatch::new("Einsum to LirMatMulUnary");
    let a = patch.tap_model(model, node.inputs[0])?;
    let b = patch.tap_model(model, node.inputs[1])?;
    let pack_a = MatMatMulPack { packer: mmm.a_pack(), k_axis: a_k, mn_axis: a_m };
    let pack_b = MatMatMulPack { packer: mmm.b_pack(), k_axis: b_k, mn_axis: b_n };
    let pa = patch.wire_node(format!("{name}.pack_a"), pack_a, &[a])?[0];
    let pb = patch.wire_node(format!("{name}.pack_b"), pack_b, &[b])?[0];

    let mut c_to_a_axis_mapping = tvec!();
    let mut c_to_b_axis_mapping = tvec!();
    for axis in op.axes.iter_all_axes().filter(|&axis| ![m_axis, k_axis, n_axis].contains(&axis)) {
        if let (&[c], &[a]) = (&*axis.outputs[0], &*axis.inputs[0]) {
            if input_facts[0].shape[a] != 1.to_dim() {
                let a = a - (a > a_m) as usize - (a > a_k) as usize;
                c_to_a_axis_mapping.push((c, a));
            }
        }
        if let (&[c], &[b]) = (&*axis.outputs[0], &*axis.inputs[1]) {
            if input_facts[1].shape[b] != 1.to_dim() {
                let b = b - (b > b_n) as usize - (b > b_k) as usize;
                c_to_b_axis_mapping.push((c, b));
            }
        }
    }

    let c_fact = op.output_facts(&input_facts)?.remove(0);
    let name = &node.name;
    let geo = AddMatMulGeometry {
        k: k.to_dim(),
        a_storage: None,
        b_storage: None,
        mmm: mmm.clone(),
        c_to_a_axis_mapping: MapOutputAxisToInput(c_to_a_axis_mapping),
        c_to_b_axis_mapping: MapOutputAxisToInput(c_to_b_axis_mapping),
    };
    let output = unsafe { mmm.c_view(c_m, c_n) };
    let lir = LirMatMulUnary::new(
        mmm,
        c_fact,
        c_m,
        c_n,
        vec![ProtoFusedSpec::AddMatMul(geo, 0, 1), ProtoFusedSpec::Store(output)],
    )
    .context("Creating LirMatMulUnary")?;
    let output = patch.wire_node(name, lir, &[pa, pb])?[0];
    patch.shunt_outside(model, node.id.into(), output)?;
    Ok(Some(patch))
}
