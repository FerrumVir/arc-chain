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

    // Grid + Color → Indices (ofcolor)
    for &c in colors {
        let name: &'static str = Box::leak(format!("ofcolor_{c}").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Grid],
            output_type: DagType::Indices,
            apply: Box::new(move |args| {
                let g = args[0].as_grid()?;
                Some(DagValue::Indices(grid::ofcolor(g, c)))
            }),
        });
    }

    // Indices → Indices transformations
    cat.push(TypedPrimitive {
        name: "idx_backdrop",
        input_types: vec![DagType::Indices],
        output_type: DagType::Indices,
        apply: Box::new(|args| {
            if let DagValue::Indices(idx) = &args[0] {
                // Backdrop of indices = bounding box
                if idx.is_empty() { return Some(DagValue::Indices(PosSet::new())); }
                let min_r = idx.iter().map(|p| p.0).min().unwrap();
                let max_r = idx.iter().map(|p| p.0).max().unwrap();
                let min_c = idx.iter().map(|p| p.1).min().unwrap();
                let max_c = idx.iter().map(|p| p.1).max().unwrap();
                let mut bd = PosSet::new();
                for r in min_r..=max_r { for c in min_c..=max_c { bd.insert((r, c)); } }
                Some(DagValue::Indices(bd))
            } else { None }
        }),
    });
    cat.push(TypedPrimitive {
        name: "idx_delta",
        input_types: vec![DagType::Indices],
        output_type: DagType::Indices,
        apply: Box::new(|args| {
            if let DagValue::Indices(idx) = &args[0] {
                if idx.is_empty() { return Some(DagValue::Indices(PosSet::new())); }
                let min_r = idx.iter().map(|p| p.0).min().unwrap();
                let max_r = idx.iter().map(|p| p.0).max().unwrap();
                let min_c = idx.iter().map(|p| p.1).min().unwrap();
                let max_c = idx.iter().map(|p| p.1).max().unwrap();
                let mut d = PosSet::new();
                for r in min_r..=max_r { for c in min_c..=max_c {
                    if !idx.contains(&(r, c)) { d.insert((r, c)); }
                }}
                Some(DagValue::Indices(d))
            } else { None }
        }),
    });
    cat.push(TypedPrimitive {
        name: "idx_neighbors",
        input_types: vec![DagType::Indices],
        output_type: DagType::Indices,
        apply: Box::new(|args| {
            if let DagValue::Indices(idx) = &args[0] {
                Some(DagValue::Indices(object::mapply_neighbors(idx)))
            } else { None }
        }),
    });

    // Object → Indices (more region functions)
    for (name, f) in [
        ("obj_box", object::obj_box as fn(&crate::Object) -> PosSet),
        ("corners", object::corners as fn(&crate::Object) -> PosSet),
        ("inbox", object::inbox as fn(&crate::Object) -> PosSet),
        ("outbox", object::outbox as fn(&crate::Object) -> PosSet),
    ] {
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Object],
            output_type: DagType::Indices,
            apply: Box::new(move |args| {
                if let DagValue::Object(obj) = &args[0] {
                    Some(DagValue::Indices(f(obj)))
                } else { None }
            }),
        });
    }

    // Objects → merged Indices via region functions
    for (name, region_fn) in [
        ("mapply_delta", object::delta as fn(&crate::Object) -> PosSet),
        ("mapply_backdrop", object::backdrop as fn(&crate::Object) -> PosSet),
        ("mapply_box", object::obj_box as fn(&crate::Object) -> PosSet),
        ("mapply_corners", object::corners as fn(&crate::Object) -> PosSet),
        ("mapply_inbox", object::inbox as fn(&crate::Object) -> PosSet),
        ("mapply_outbox", object::outbox as fn(&crate::Object) -> PosSet),
    ] {
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Objects],
            output_type: DagType::Indices,
            apply: Box::new(move |args| {
                if let DagValue::Objects(objs) = &args[0] {
                    let mut all = PosSet::new();
                    for obj in objs { all.extend(region_fn(obj)); }
                    Some(DagValue::Indices(all))
                } else { None }
            }),
        });
    }

    // Objects → merged neighbor indices
    cat.push(TypedPrimitive {
        name: "mapply_neighbors",
        input_types: vec![DagType::Objects],
        output_type: DagType::Indices,
        apply: Box::new(|args| {
            if let DagValue::Objects(objs) = &args[0] {
                let mut all = PosSet::new();
                for obj in objs {
                    all.extend(object::mapply_neighbors(&obj.positions()));
                }
                Some(DagValue::Indices(all))
            } else { None }
        }),
    });

    // Indices + Grid + Color → Grid (fill)
    for &c in colors {
        let name: &'static str = Box::leak(format!("fill_idx_{c}").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Indices, DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                if let (DagValue::Indices(idx), DagValue::Grid(g)) = (&args[0], &args[1]) {
                    Some(DagValue::Grid(grid::fill(g, c, idx)))
                } else { None }
            }),
        });
        let name: &'static str = Box::leak(format!("underfill_idx_{c}").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Indices, DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                if let (DagValue::Indices(idx), DagValue::Grid(g)) = (&args[0], &args[1]) {
                    Some(DagValue::Grid(grid::underfill(g, c, idx)))
                } else { None }
            }),
        });
    }

    // Objects → Object: colorfilter + first/argmax
    for &c in colors {
        let name: &'static str = Box::leak(format!("cf{c}_argmax").into_boxed_str());
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Objects],
            output_type: DagType::Object,
            apply: Box::new(move |args| {
                if let DagValue::Objects(objs) = &args[0] {
                    let filtered = object::colorfilter(objs, c);
                    object::argmax_size(&filtered).cloned().map(DagValue::Object)
                } else { None }
            }),
        });
    }

    // Object → Grid: cover (erase object)
    cat.push(TypedPrimitive {
        name: "cover",
        input_types: vec![DagType::Object, DagType::Grid],
        output_type: DagType::Grid,
        apply: Box::new(|args| {
            if let (DagValue::Object(obj), DagValue::Grid(g)) = (&args[0], &args[1]) {
                Some(DagValue::Grid(object::cover(g, obj)))
            } else { None }
        }),
    });

    // Object movement (fixed offsets)
    for (dr, dc, name) in [
        (1isize, 0isize, "move_down"), (-1, 0, "move_up"),
        (0, 1, "move_right"), (0, -1, "move_left"),
    ] {
        cat.push(TypedPrimitive {
            name,
            input_types: vec![DagType::Object, DagType::Grid],
            output_type: DagType::Grid,
            apply: Box::new(move |args| {
                if let (DagValue::Object(obj), DagValue::Grid(g)) = (&args[0], &args[1]) {
                    Some(DagValue::Grid(object::move_obj(g, obj, dr, dc)))
                } else { None }
            }),
        });
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
