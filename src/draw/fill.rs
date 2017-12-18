use draw::figure::Figure;
use draw::png_image::RgbaColor;
use draw::point::Point;
use mapcss::color::Color;

use std::cmp::{min, max};
use std::collections::HashMap;

pub fn fill_contour<I>(points: I, color: &Color, opacity: f64) -> Figure
    where I: Iterator<Item=(Point, Point)>
{
    let mut figure: Figure = Default::default();
    let mut y_to_edges = Default::default();
    let fill_color = RgbaColor::from_color(color, opacity);

    for (idx, (p1, p2)) in points.enumerate() {
        draw_line(idx, &p1, &p2, &mut y_to_edges);
    }

    for (y, edges) in &y_to_edges {
        let mut good_edges = edges.values().filter(|e| !e.is_poisoned).collect::<Vec<_>>();
        good_edges.sort_by_key(|e| e.x_min);

        let mut idx = 0;
        while idx < good_edges.len() {
            let e1 = good_edges[idx];
            let e2 = good_edges[idx + 1];
            for x in e1.x_min .. (e2.x_max + 1) {
                figure.add(x as usize, *y as usize, fill_color.clone());
            }
            idx += 2;
        }
    }

    figure
}

// Stripped-down version of Bresenham which is extremely easy to implement.
// See http://members.chello.at/~easyfilter/bresenham.html
fn draw_line(edge_idx: usize, p1: &Point, p2: &Point, y_to_edges: &mut EdgesByY) {
    let dx = (p2.x - p1.x).abs();
    let dy = -(p2.y - p1.y).abs();

    let get_dir = |c1, c2| if c1 < c2 { 1 } else { -1 };
    let sx = get_dir(p1.x, p2.x);
    let sy = get_dir(p1.y, p2.y);

    let mut err = dx + dy;
    let mut cur_point = p1.clone();

    loop {
        let is_start = cur_point == *p1;
        let is_end = cur_point == *p2;

        let is_poisoned = if is_start {
            p1.y <= p2.y
        } else if is_end {
            p2.y <= p1.y
        } else {
            false
        };

        let edge = y_to_edges
            .entry(cur_point.y)
            .or_insert_with(Default::default)
            .entry(edge_idx)
            .or_insert_with(|| Edge {
                x_min: cur_point.x,
                x_max: cur_point.x,
                is_poisoned,
            });

        edge.x_min = min(edge.x_min, cur_point.x);
        edge.x_max = max(edge.x_max, cur_point.x);
        edge.is_poisoned |= is_poisoned;

        if is_end {
            break;
        }

        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            cur_point.x += sx;
        }
        if e2 <= dx {
            err += dx;
            cur_point.y += sy;
        }
    }
}

type EdgesByY = HashMap<i32, HashMap<usize, Edge>>;

struct Edge {
    x_min: i32,
    x_max: i32,
    is_poisoned: bool,
}