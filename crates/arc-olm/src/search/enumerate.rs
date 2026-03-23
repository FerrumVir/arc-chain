//! Typed program enumeration.

use crate::{Grid, Color, PosSet};
use crate::primitives::grid;
use crate::primitives::object;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DagType { Grid, Objects, Object, Indices, Color, Int }

#[derive(Clone, Debug)]
pub enum DagValue {
    Grid(Grid),
    Objects(Vec<crate::Object>),
    Object(crate::Object),
    Indices(PosSet),
    Color(Color),
    Int(usize),
}

impl DagValue {
    pub fn dag_type(&self) -> DagType {
        match self {
            DagValue::Grid(_) => DagType::Grid,
            DagValue::Objects(_) => DagType::Objects,
            DagValue::Object(_) => DagType::Object,
            DagValue::Indices(_) => DagType::Indices,
            DagValue::Color(_) => DagType::Color,
            DagValue::Int(_) => DagType::Int,
        }
    }
    pub fn as_grid(&self) -> Option<&Grid> {
        if let DagValue::Grid(g) = self { Some(g) } else { None }
    }
}

pub struct TypedPrimitive {
    pub name: &'static str,
    pub input_types: Vec<DagType>,
    pub output_type: DagType,
    pub apply: Box<dyn Fn(&[DagValue]) -> Option<DagValue> + Send + Sync>,
}

pub fn build_primitive_catalog(colors: &[Color]) -> Vec<TypedPrimitive> {
    let mut cat: Vec<TypedPrimitive> = Vec::new();

    // Unary Grid → Grid
    let g2g: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("rot90", grid::rot90), ("rot180", grid::rot180), ("rot270", grid::rot270),
        ("hmirror", grid::hmirror), ("vmirror", grid::vmirror),
        ("dmirror", grid::dmirror), ("cmirror", grid::cmirror),
        ("tophalf", grid::tophalf), ("bottomhalf", grid::bottomhalf),
        ("lefthalf", grid::lefthalf), ("righthalf", grid::righthalf),
        ("compress", grid::compress), ("trim", grid::trim),
    ];
    for (name, f) in g2g {
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                Some(DagValue::Grid(f(args[0].as_grid()?)))
            }),
        });
    }

    // Self-concat patterns
    let concat_patterns: Vec<(&str, fn(&Grid) -> Grid)> = vec![
        ("hconcat_self", |g| grid::hconcat(g, g)),
        ("vconcat_self", |g| grid::vconcat(g, g)),
        ("hconcat_vm", |g| grid::hconcat(g, &grid::vmirror(g))),
        ("vconcat_hm", |g| grid::vconcat(g, &grid::hmirror(g))),
        ("hconcat_vm_r", |g| grid::hconcat(&grid::vmirror(g), g)),
        ("vconcat_hm_r", |g| grid::vconcat(&grid::hmirror(g), g)),
    ];
    for (name, f) in concat_patterns {
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                Some(DagValue::Grid(f(args[0].as_grid()?)))
            }),
        });
    }

    // Object extraction
    for (uni, diag, nobg, name) in [
        (true, true, true, "obj_TTT"),
        (true, false, true, "obj_TFT"),
        (false, true, true, "obj_FTT"),
        (false, false, true, "obj_FFT"),
    ] {
        let name: &'static str = name;
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Objects,
            apply: Box::new(move |args| {
                let g = args[0].as_grid()?;
                Some(DagValue::Objects(object::objects(g, uni, diag, nobg)))
            }),
        });
    }

    // Object selectors
    cat.push(TypedPrimitive {
        name: "argmax_size",
        input_types: vec![DagType::Objects],
        output_type: DagType::Object,
        apply: Box::new(|args| {
            if let DagValue::Objects(objs) = &args[0] {
                object::argmax_size(objs).cloned().map(DagValue::Object)
            } else { None }
        }),
    });
    cat.push(TypedPrimitive {
        name: "argmin_size",
        input_types: vec![DagType::Objects],
        output_type: DagType::Object,
        apply: Box::new(|args| {
            if let DagValue::Objects(objs) = &args[0] {
                object::argmin_size(objs).cloned().map(DagValue::Object)
            } else { None }
        }),
    });

    // Object + Grid → Grid (subgrid)
    cat.push(TypedPrimitive {
        name: "subgrid",
        input_types: vec![DagType::Object, DagType::Grid],
        output_type: DagType::Grid,
        apply: Box::new(|args| {
            if let (DagValue::Object(obj), DagValue::Grid(g)) = (&args[0], &args[1]) {
                Some(DagValue::Grid(object::subgrid(obj, g)))
            } else { None }
        }),
    });

    // Parameterized: replace colors
    for &c1 in colors {
        for &c2 in colors {
            if c1 == c2 { continue; }
            let name: &'static str = Box::leak(format!("replace_{c1}_{c2}").into_boxed_str());
            cat.push(TypedPrimitive {
                name,
                input_types: vec![DagType::Grid],
                output_type: DagType::Grid,
                apply: Box::new(move |args| {
                    Some(DagValue::Grid(grid::replace_color(args[0].as_grid()?, c1, c2)))
                }),
            });
        }
    }

    // Upscale/downscale
    for factor in [2usize, 3, 4, 5] {
        let name: &'static str = Box::leak(format!("upscale_{factor}").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                Some(DagValue::Grid(grid::upscale(args[0].as_grid()?, factor)))
            }),
        });
        let name: &'static str = Box::leak(format!("downscale_{factor}").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                Some(DagValue::Grid(grid::downscale(args[0].as_grid()?, factor)))
            }),
        });
    }

    cat
}

#[derive(Clone, Debug)]
pub struct PartialProgram {
    pub steps: Vec<&'static str>,
    pub current_value: DagValue,
    pub current_type: DagType,
    pub fitness: f64,
    pub hash: u64,
}

pub fn compute_fitness(result: &Grid, target: &Grid) -> f64 {
    if result.len() != target.len() || result.is_empty() { return 0.0; }
    if result[0].len() != target[0].len() { return 0.0; }
    let total = result.len() * result[0].len();
    let matching: usize = result.iter().zip(target.iter())
        .map(|(rr, tr)| rr.iter().zip(tr.iter()).filter(|(a, b)| a == b).count())
        .sum();
    matching as f64 / total as f64
}

pub fn quick_hash(grid: &Grid) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    grid.hash(&mut hasher);
    hasher.finish()
}
