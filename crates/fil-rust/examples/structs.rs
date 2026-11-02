//! C structs as Rust types: by value, through pointers, holding pointers, and
//! data structures built by C and walked by Rust.
//!
//!     cargo run --example structs

mod support;

use std::ffi::c_char;

use fil_rust::{FilPtr, FilType, Ptr};

/// `struct point`. The derive computes the C layout and checks it against
/// the `#[fil(...)]` assertion at compile time.
#[derive(FilType, Clone, Copy, Debug, PartialEq)]
#[fil(size = 16, align = 8)]
struct Point {
    x: f64,
    y: f64,
}

/// `struct polygon`. Pointer fields carry the scope lifetime `'f`, so a
/// `Polygon` read from Fil-C memory can't outlive the scope it was read in.
#[derive(FilType, Clone, Copy, Debug)]
#[fil(size = 24)]
struct Polygon<'f> {
    name: FilPtr<'f, c_char>,
    points: FilPtr<'f, Point>,
    count: usize,
}

/// `struct node`: a self-referential type. Field types use `'static` (as in
/// signatures); the values you read have the scope's lifetime.
#[derive(FilType, Clone, Copy, Debug)]
struct Node<'f> {
    value: i32,
    next: FilPtr<'f, Node<'static>>,
}

fil_rust::foreign! {
    struct Shapes {
        fn midpoint(a: Point, b: Point) -> Point;
        fn polygon_area(p: Ptr<Polygon<'static>>) -> f64;
        fn polygon_scale(p: Ptr<Polygon<'static>>, k: f64);
        fn regular(name: Ptr<c_char>, n: usize, radius: f64) -> Polygon<'static>;
        fn squares(n: i32) -> Ptr<Node<'static>>;
    }
}

fn main() -> fil_rust::Result<()> {
    let rt = support::runtime();
    let path = support::build("shapes.c");
    rt.run(|fil| -> fil_rust::Result<()> {
        let shapes = Shapes::load(fil, &path)?;

        // Small structs travel by value.
        let m = shapes.midpoint(fil, Point { x: 0.0, y: 0.0 }, Point { x: 4.0, y: 2.0 })?;
        println!("midpoint: {m:?}");

        // A struct holding pointers, built in Rust.
        let corners = [
            Point { x: 0.0, y: 0.0 },
            Point { x: 3.0, y: 0.0 },
            Point { x: 3.0, y: 2.0 },
            Point { x: 0.0, y: 2.0 },
        ];
        let rect = Polygon {
            name: fil.c_str("rectangle")?,
            points: fil.alloc_array::<Point>(&corners)?,
            count: corners.len(),
        };
        let rect_ptr = fil.alloc::<Polygon>(&rect)?;
        println!("{} area: {}", rect.name.read_string(fil)?, shapes.polygon_area(fil, rect_ptr)?);

        // C mutates it through the pointer; we see the change.
        shapes.polygon_scale(fil, rect_ptr, 2.0)?;
        let scaled = rect_ptr.read(fil)?;
        println!("scaled corners: {:?}", scaled.points.read_array(fil, scaled.count)?);
        println!("scaled area: {}", shapes.polygon_area(fil, rect_ptr)?);

        // A struct returned by value whose pointers C allocated.
        let hex = shapes.regular(fil, fil.c_str("hexagon")?, 6, 1.0)?;
        println!(
            "{} with {} points, area {:.4}",
            hex.name.read_string(fil)?,
            hex.count,
            shapes.polygon_area(fil, fil.alloc::<Polygon>(&hex)?)?
        );
        // Reading past the end of what C allocated is an error, not UB.
        match hex.points.read_at(fil, hex.count) {
            Err(e) => println!("reading point {}: {e}", hex.count),
            Ok(p) => println!("unexpectedly read {p:?}"),
        }

        // Walk a linked list that C built.
        let mut values = Vec::new();
        let mut cursor = shapes.squares(fil, 8)?;
        while !cursor.is_null() {
            let node = cursor.read(fil)?;
            values.push(node.value);
            cursor = node.next;
        }
        println!("squares: {values:?}");
        Ok(())
    })
}
