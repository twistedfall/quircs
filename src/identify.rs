use std::cell::RefCell;
use std::rc::Rc;

use crate::quirc::*;
use crate::version_db::*;

#[derive(Copy, Clone)]
#[repr(C)]
struct Neighbour {
    pub index: i32,
    pub distance: f64,
}

#[derive(Copy, Clone)]
#[repr(C)]
struct NeighbourList {
    pub n: [Neighbour; 32],
    pub count: usize,
}

struct PolygonScoreData<'a> {
    pub ref_0: Point,
    pub scores: [i32; 4],
    pub corners: &'a mut [Point; 4],
}

// ---  Linear algebra routines

fn line_intersect(p0: &Point, p1: &Point, q0: &Point, q1: &Point, r: &mut Point) -> i32 {
    /* (a, b) is perpendicular to line p */
    let a = -(p1.y - p0.y);
    let b = p1.x - p0.x;

    /* (c, d) is perpendicular to line q */
    let c = -(q1.y - q0.y);
    let d = q1.x - q0.x;

    /* e and f are dot products of the respective vectors with p and q */
    let e = a * p1.x + b * p1.y;
    let f = c * q1.x + d * q1.y;

    /* Now we need to solve:
     *     [a b] [rx]   [e]
     *     [c d] [ry] = [f]
     *
     * We do this by inverting the matrix and applying it to (e, f):
     *       [ d -b] [e]   [rx]
     * 1/det [-c  a] [f] = [ry]
     */
    let det = a * d - b * c;
    if det == 0 {
        return 0;
    }
    r.x = (d * e - b * f) / det;
    r.y = (-c * e + a * f) / det;

    1
}

fn perspective_setup(c: &mut [f64; 8], rect: &[Point; 4], w: f64, h: f64) {
    let x0 = rect[0].x as f64;
    let y0 = rect[0].y as f64;
    let x1 = rect[1].x as f64;
    let y1 = rect[1].y as f64;
    let x2 = rect[2].x as f64;
    let y2 = rect[2].y as f64;
    let x3 = rect[3].x as f64;
    let y3 = rect[3].y as f64;

    let wden = w * (x2 * y3 - x3 * y2 + (x3 - x2) * y1 + x1 * (y2 - y3));
    let hden = h * (x2 * y3 + x1 * (y2 - y3) - x3 * y2 + (x3 - x2) * y1);

    c[0] = (x1 * (x2 * y3 - x3 * y2)
        + x0 * (-x2 * y3 + x3 * y2 + (x2 - x3) * y1)
        + x1 * (x3 - x2) * y0)
        / wden;
    c[1] = -(x0 * (x2 * y3 + x1 * (y2 - y3) - x2 * y1) - x1 * x3 * y2
        + x2 * x3 * y1
        + (x1 * x3 - x2 * x3) * y0)
        / hden;
    c[2] = x0;
    c[3] = (y0 * (x1 * (y3 - y2) - x2 * y3 + x3 * y2)
        + y1 * (x2 * y3 - x3 * y2)
        + x0 * y1 * (y2 - y3))
        / wden;
    c[4] = (x0 * (y1 * y3 - y2 * y3) + x1 * y2 * y3 - x2 * y1 * y3
        + y0 * (x3 * y2 - x1 * y2 + (x2 - x3) * y1))
        / hden;
    c[5] = y0;
    c[6] = (x1 * (y3 - y2) + x0 * (y2 - y3) + (x2 - x3) * y1 + (x3 - x2) * y0) / wden;
    c[7] = (-x2 * y3 + x1 * y3 + x3 * y2 + x0 * (y1 - y2) - x3 * y1 + (x2 - x1) * y0) / hden;
}

fn perspective_map(c: &[f64; 8], u: f64, v: f64, ret: &mut Point) {
    let den = c[6] * u + c[7] * v + 1.0f64;
    let x = (c[0] * u + c[1] * v + c[2]) / den;
    let y = (c[3] * u + c[4] * v + c[5]) / den;

    ret.x = x.round() as i32;
    ret.y = y.round() as i32;
}

fn perspective_unmap(c: &[f64; 8], in_0: &Point, u: &mut f64, v: &mut f64) {
    let x = in_0.x as f64;
    let y = in_0.y as f64;

    let den = -c[0] * c[7] * y + c[1] * c[6] * y + (c[3] * c[7] - c[4] * c[6]) * x + c[0] * c[4]
        - c[1] * c[3];
    *u = -(c[1] * (y - c[5]) - c[2] * c[7] * y + (c[5] * c[7] - c[4]) * x + c[2] * c[4]) / den;
    *v = (c[0] * (y - c[5]) - c[2] * c[6] * y + (c[5] * c[6] - c[3]) * x + c[2] * c[3]) / den;
}

// --- Span-based floodfill routine

const FLOOD_FILL_MAX_DEPTH: i32 = 4096;

#[derive(Clone)]
enum UserData<'a> {
    Region(Rc<RefCell<&'a mut Region>>),
    Polygon(Rc<RefCell<&'a mut PolygonScoreData<'a>>>),
    None,
}

unsafe fn flood_fill_seed<F>(
    q: *mut Quirc,
    x: i32,
    y: i32,
    from: i32,
    to: i32,
    func: Option<&F>,
    user_data: UserData<'_>,
    depth: i32,
) where
    F: Fn(UserData<'_>, i32, i32, i32),
{
    if depth >= FLOOD_FILL_MAX_DEPTH {
        return;
    }

    let mut left = x as usize;
    let mut right = x as usize;

    let width = (*q).w;
    let to_range = |start| start..start + width;

    let mut row = &mut (*q).pixels[to_range((y as usize) * (*q).w)];
    while left > 0 && row[left - 1] as i32 == from {
        left -= 1;
    }
    while right < (*q).w - 1 && row[right + 1] as i32 == from {
        right += 1;
    }

    /* Fill the extent */
    for val in &mut row[left..=right] {
        *val = to as Pixel;
    }
    if func.is_some() {
        func.expect("non-null function pointer")(user_data.clone(), y, left as i32, right as i32);
    }

    /* Seed new flood-fills */
    if y > 0 {
        row = &mut (*q).pixels[to_range(((y - 1) * (*q).w as i32) as usize)];
        for (i, val) in row.iter_mut().enumerate().skip(left).take(right - left + 1) {
            if *val as i32 == from {
                flood_fill_seed(
                    q,
                    i as i32,
                    y - 1,
                    from,
                    to,
                    func,
                    user_data.clone(),
                    depth + 1,
                );
            }
        }
    }

    if y < (*q).h as i32 - 1 {
        row = &mut (*q).pixels[to_range(((y + 1) * (*q).w as i32) as usize)];
        for (i, val) in row.iter_mut().enumerate().skip(left).take(right - left + 1) {
            if *val as i32 == from {
                flood_fill_seed(
                    q,
                    i as i32,
                    y + 1,
                    from,
                    to,
                    func,
                    user_data.clone(),
                    depth + 1,
                );
            }
        }
    }
}

// --- Adaptive thresholding

unsafe fn otsu(q: *const Quirc) -> u8 {
    let num_pixels = (*q).w * (*q).h;

    // Calculate histogram
    let mut histogram: [u32; 256] = [0; 256];
    let image = &(*q).image;

    for value in image {
        let value = *value as usize;
        histogram[value] = histogram[value].wrapping_add(1);
    }

    // Calculate weighted sum of histogram values
    let mut sum: u32 = 0;
    for (i, val) in histogram.iter().enumerate() {
        sum = sum.wrapping_add((i as u32).wrapping_mul(*val));
    }

    // Compute threshold
    let mut sum_b: i32 = 0;
    let mut q1: i32 = 0;
    let mut max: f64 = 0 as f64;
    let mut threshold: u8 = 0 as u8;
    for (i, val) in histogram.iter().enumerate() {
        // Weighted background
        q1 = (q1 as u32).wrapping_add(*val) as i32 as i32;
        if q1 == 0 {
            continue;
        }
        // Weighted foreground
        let q2 = num_pixels as i32 - q1;
        if q2 == 0 {
            break;
        }
        sum_b = (sum_b as u32).wrapping_add((i as u32).wrapping_mul(*val)) as i32 as i32;
        let m1 = sum_b as f64 / q1 as f64;
        let m2 = (sum as f64 - sum_b as f64) / q2 as f64;
        let m1m2 = m1 - m2;
        let variance = m1m2 * m1m2 * q1 as f64 * q2 as f64;
        if variance >= max {
            threshold = i as u8;
            max = variance
        }
    }

    threshold
}

fn area_count(user_data: UserData<'_>, _y: i32, left: i32, right: i32) {
    if let UserData::Region(ref region) = user_data {
        region.borrow_mut().count += right - left + 1;
    } else {
        panic!("invalid user data");
    }
}

unsafe fn region_code(q: *mut Quirc, x: i32, y: i32) -> i32 {
    if x < 0 || y < 0 || x >= (*q).w as i32 || y >= (*q).h as i32 {
        return -1;
    }
    let pixel = (*q).pixels[(y * (*q).w as i32 + x) as usize] as i32;
    if pixel >= 2 {
        return pixel;
    }
    if pixel == 0 {
        return -1;
    }
    let region = (*q).num_regions() as i32;

    if region >= 65534 {
        return -1;
    }

    (*q).regions.push(Region {
        seed: Point { x, y },
        count: 0,
        capstone: -1,
    });

    flood_fill_seed(
        q,
        x,
        y,
        pixel,
        region,
        Some(&area_count),
        UserData::Region(Rc::new(RefCell::new(&mut (*q).regions[region as usize]))),
        0,
    );

    region
}

fn find_one_corner(user_data: UserData<'_>, y: i32, left: i32, right: i32) {
    if let UserData::Polygon(ref psd) = user_data {
        let mut psd = psd.borrow_mut();
        let xs: [i32; 2] = [left, right];
        let dy: i32 = y - psd.ref_0.y;

        for x in &xs {
            let dx = *x - (*psd).ref_0.x;
            let d = dx * dx + dy * dy;
            if d > psd.scores[0] {
                psd.scores[0] = d;
                psd.corners[0].x = *x;
                psd.corners[0].y = y;
            }
        }
    } else {
        panic!("invalid user data");
    }
}

fn find_other_corners(user_data: UserData<'_>, y: i32, left: i32, right: i32) {
    if let UserData::Polygon(ref psd) = user_data {
        let mut psd = psd.borrow_mut();
        let xs: [i32; 2] = [left, right];

        for x in &xs {
            let up = *x * psd.ref_0.x + y * psd.ref_0.y;
            let right_0 = *x * -psd.ref_0.y + y * psd.ref_0.x;
            let scores: [i32; 4] = [up, right_0, -up, -right_0];

            for j in 0..4 {
                if scores[j] > psd.scores[j] {
                    psd.scores[j] = scores[j];
                    psd.corners[j].x = *x;
                    psd.corners[j].y = y;
                }
            }
        }
    } else {
        panic!("invalid user data");
    }
}

unsafe fn find_region_corners(q: *mut Quirc, rcode: i32, point: &Point, corners: &mut [Point; 4]) {
    let region = (*q).regions[rcode as usize];
    let mut psd = PolygonScoreData {
        ref_0: *point,
        scores: [-1, 0, 0, 0],
        corners,
    };
    let psd_ref = Rc::new(RefCell::new(&mut psd));
    flood_fill_seed(
        q,
        region.seed.x,
        region.seed.y,
        rcode,
        1,
        Some(&find_one_corner),
        UserData::Polygon(psd_ref.clone()),
        0,
    );
    // Safe to unwrap, because the only reference was given to the call
    // to flood_fill_seed above.
    let mut psd = Rc::try_unwrap(psd_ref)
        .map_err(|_| ())
        .unwrap()
        .into_inner();
    psd.ref_0.x = psd.corners[0].x - psd.ref_0.x;
    psd.ref_0.y = psd.corners[0].y - psd.ref_0.y;
    for corner in &mut psd.corners[..] {
        *corner = region.seed;
    }

    let i = region.seed.x * psd.ref_0.x + region.seed.y * psd.ref_0.y;
    psd.scores[0] = i;
    psd.scores[2] = -i;

    let i = region.seed.x * -psd.ref_0.y + region.seed.y * psd.ref_0.x;
    psd.scores[1] = i;
    psd.scores[3] = -i;

    flood_fill_seed(
        q,
        region.seed.x,
        region.seed.y,
        1,
        rcode,
        Some(&find_other_corners),
        UserData::Polygon(Rc::new(RefCell::new(&mut psd))),
        0,
    );
}

unsafe fn record_capstone(q: *mut Quirc, ring: i32, stone: i32) {
    let stone_reg = &mut (*q).regions[stone as usize];
    let ring_reg = &mut (*q).regions[ring as usize];
    if (*q).num_capstones() >= 32 {
        return;
    }
    let cs_index = (*q).num_capstones() as i32;

    let mut capstone = Capstone::default();
    capstone.qr_grid = -1;
    capstone.ring = ring;
    capstone.stone = stone;
    (*q).capstones.push(capstone);
    let capstone = &mut (*q).capstones[cs_index as usize];

    stone_reg.capstone = cs_index;
    ring_reg.capstone = cs_index;

    /* Find the corners of the ring */
    find_region_corners(q, ring, &(*stone_reg).seed, &mut capstone.corners);

    /* Set up the perspective transform and find the center */
    perspective_setup(&mut capstone.c, &capstone.corners, 7.0, 7.0);
    perspective_map(&capstone.c, 3.5, 3.5, &mut capstone.center);
}

unsafe fn test_capstone(q: *mut Quirc, x: i32, y: i32, pb: &[i32]) {
    let ring_right = region_code(q, x - pb[4], y);
    let stone = region_code(q, x - pb[4] - pb[3] - pb[2], y);
    let ring_left = region_code(q, x - pb[4] - pb[3] - pb[2] - pb[1] - pb[0], y);
    if ring_left < 0 || ring_right < 0 || stone < 0 {
        return;
    }
    /* Left and ring of ring should be connected */
    if ring_left != ring_right {
        return;
    }
    /* Ring should be disconnected from stone */
    if ring_left == stone {
        return;
    }
    let stone_reg = &mut (*q).regions[stone as usize];
    let ring_reg = &mut (*q).regions[ring_left as usize];

    /* Already detected */
    if stone_reg.capstone >= 0 || ring_reg.capstone >= 0 {
        return;
    }
    /* Ratio should ideally be 37.5 */
    let ratio = stone_reg.count * 100 / ring_reg.count;
    if ratio < 10 || ratio > 70 {
        return;
    }
    record_capstone(q, ring_left, stone);
}

unsafe fn finder_scan(q: *mut Quirc, y: usize) {
    static CHECK: [i32; 5] = [1, 1, 3, 1, 1];

    let start = (y * (*q).w) as usize;
    let row = &(*q).pixels[start..start + (*q).w];
    let mut last_color = 0;
    let mut run_length = 0;
    let mut run_count = 0;
    let mut pb: [i32; 5] = [0; 5];

    for (x, pixel) in row.iter().enumerate() {
        let color = if *pixel as i32 != 0 { 1 } else { 0 };

        if x != 0 && color != last_color {
            pb.copy_within(1.., 0);
            pb[4] = run_length;
            run_length = 0;
            run_count += 1;
            if color == 0 && run_count >= 5 {
                let mut ok = 1;
                let avg = (pb[0] + pb[1] + pb[3] + pb[4]) / 4;
                let err = avg * 3 / 4;

                for (pb, check) in pb.iter().zip(CHECK.iter()) {
                    if *pb < *check * avg - err || *pb > *check * avg + err {
                        ok = 0;
                    }
                }

                if ok != 0 {
                    test_capstone(q, x as i32, y as i32, &pb);
                }
            }
        }

        run_length += 1;
        last_color = color;
    }
}

unsafe fn find_alignment_pattern(q: *mut Quirc, index: usize) {
    let mut qr = &mut (*q).grids[index];
    let c0 = &mut (*q).capstones[qr.caps[0]];
    let c2 = &mut (*q).capstones[qr.caps[2]];

    let mut a = Point::default();
    let mut c = Point::default();
    let mut step_size = 1;
    let mut dir = 0;
    let mut u = 0.;
    let mut v = 0.;

    /* Grab our previous estimate of the alignment pattern corner */
    let mut b = qr.align;

    /* Guess another two corners of the alignment pattern so that we
     * can estimate its size.
     */
    perspective_unmap(&(*c0).c, &mut b, &mut u, &mut v);
    perspective_map(&(*c0).c, u, v + 1.0f64, &mut a);
    perspective_unmap(&(*c2).c, &mut b, &mut u, &mut v);
    perspective_map(&(*c2).c, u + 1.0f64, v, &mut c);
    let size_estimate = ((a.x - b.x) * -(c.y - b.y) + (a.y - b.y) * (c.x - b.x)).abs();

    /* Spiral outwards from the estimate point until we find something
     * roughly the right size. Don't look too far from the estimate
     * point.
     */
    static DX_MAP: [i32; 4] = [1, 0, -1, 0];
    static DY_MAP: [i32; 4] = [0, -1, 0, 1];

    while step_size * step_size < size_estimate * 100 {
        for _ in 0..step_size {
            let code = region_code(q, b.x, b.y);
            if code >= 0 {
                let reg = &mut (*q).regions[code as usize];
                if reg.count >= size_estimate / 2 && reg.count <= size_estimate * 2 {
                    qr.align_region = code;
                    return;
                }
            }
            b.x += DX_MAP[dir as usize];
            b.y += DY_MAP[dir as usize];
        }

        dir = (dir + 1) % 4;
        if dir & 1 == 0 {
            step_size += 1
        }
    }
}

fn find_leftmost_to_line(user_data: UserData<'_>, y: i32, left: i32, right: i32) {
    if let UserData::Polygon(ref psd) = user_data {
        let mut psd = psd.borrow_mut();
        let xs: [i32; 2] = [left, right];

        for x in &xs {
            let d = -psd.ref_0.y * *x + psd.ref_0.x * y;
            if d < psd.scores[0] {
                psd.scores[0] = d;
                psd.corners[0].x = *x;
                psd.corners[0].y = y;
            }
        }
    } else {
        panic!("invalid user data");
    }
}

/// Do a Bresenham scan from one point to another and count the number
/// of black/white transitions.
unsafe fn timing_scan(q: *const Quirc, p0: *const Point, p1: *const Point) -> i32 {
    let mut n: i32 = (*p1).x - (*p0).x;
    let mut d: i32 = (*p1).y - (*p0).y;
    let mut x: i32 = (*p0).x;
    let mut y: i32 = (*p0).y;
    let dom: *mut i32;
    let nondom: *mut i32;
    let dom_step: i32;
    let nondom_step: i32;
    let mut a: i32 = 0;
    let mut run_length: i32 = 0;
    let mut count: i32 = 0;
    if (*p0).x < 0 || (*p0).y < 0 || (*p0).x >= (*q).w as i32 || (*p0).y >= (*q).h as i32 {
        return -1;
    }
    if (*p1).x < 0 || (*p1).y < 0 || (*p1).x >= (*q).w as i32 || (*p1).y >= (*q).h as i32 {
        return -1;
    }
    if n.abs() > d.abs() {
        let swap: i32 = n;
        n = d;
        d = swap;
        dom = &mut x;
        nondom = &mut y
    } else {
        dom = &mut y;
        nondom = &mut x
    }
    if n < 0 {
        n = -n;
        nondom_step = -1
    } else {
        nondom_step = 1
    }
    if d < 0 {
        d = -d;
        dom_step = -1
    } else {
        dom_step = 1
    }
    x = (*p0).x;
    y = (*p0).y;

    for _ in 0..=d {
        if y < 0 || y >= (*q).h as i32 || x < 0 || x >= (*q).w as i32 {
            break;
        }
        let pixel = (*q).pixels[(y * (*q).w as i32 + x) as usize] as i32;
        if pixel != 0 {
            if run_length >= 2 {
                count += 1;
            }
            run_length = 0;
        } else {
            run_length += 1;
        }
        a += n;
        *dom += dom_step;
        if a >= d {
            *nondom += nondom_step;
            a -= d
        }
    }

    count
}

/// Try the measure the timing pattern for a given QR code. This does
/// not require the global perspective to have been set up, but it
/// does require that the capstone corners have been set to their
/// canonical rotation.
///
/// For each capstone, we find a point in the middle of the ring band
/// which is nearest the centre of the code. Using these points, we do
/// a horizontal and a vertical timing scan.
unsafe fn measure_timing_pattern(q: *mut Quirc, index: usize) -> i32 {
    let mut qr = &mut (*q).grids[index];

    static US: [f64; 3] = [6.5, 6.5, 0.5];
    static VS: [f64; 3] = [0.5, 6.5, 6.5];

    for (i, (us, vs)) in US.iter().zip(VS.iter()).enumerate() {
        let cap = &mut (*q).capstones[qr.caps[i]];

        perspective_map(&(*cap).c, *us, *vs, &mut qr.tpep[i]);
    }

    qr.hscan = timing_scan(q, &mut qr.tpep[1], &mut qr.tpep[2]);
    qr.vscan = timing_scan(q, &mut qr.tpep[1], &mut qr.tpep[0]);

    let mut scan = qr.hscan;
    if qr.vscan > scan {
        scan = qr.vscan
    }

    /* If neither scan worked, we can't go any further. */
    if scan < 0 {
        return -1;
    }

    /* Choose the nearest allowable grid size */
    let size = scan * 2 + 13;
    let ver = (size - 15) / 4;
    qr.grid_size = ver * 4 + 17;

    0
}

/// Read a cell from a grid using the currently set perspective
/// transform. Returns +/- 1 for black/white, 0 for cells which are
/// out of image bounds.
unsafe fn read_cell(q: *const Quirc, index: usize, x: i32, y: i32) -> i32 {
    let qr = &(*q).grids[index];
    let mut p = Point::default();

    perspective_map(&qr.c, x as f64 + 0.5f64, y as f64 + 0.5f64, &mut p);
    if p.y < 0 || p.y >= (*q).h as i32 || p.x < 0 || p.x >= (*q).w as i32 {
        return 0;
    }

    if (*q).pixels[(p.y * (*q).w as i32 + p.x) as usize] != 0 {
        1
    } else {
        -1
    }
}

#[derive(Debug)]
struct Image<'a> {
    pixels: &'a [Pixel],
    width: usize,
    height: usize,
}

fn fitness_cell(qr: &Grid, image: &Image<'_>, x: i32, y: i32) -> i32 {
    static OFFSETS: [f64; 3] = [0.3, 0.5, 0.7];

    let mut score = 0;
    let mut p = Point::default();

    for v in &OFFSETS {
        for u in &OFFSETS {
            p.clear();
            perspective_map(&qr.c, x as f64 + *u, y as f64 + *v, &mut p);

            if !(p.y < 0 || p.y >= image.height as i32 || p.x < 0 || p.x >= image.width as i32) {
                if image.pixels[(p.y * image.width as i32 + p.x) as usize] != 0 {
                    score += 1;
                } else {
                    score -= 1;
                }
            }
        }
    }

    score
}

fn fitness_ring(qr: &Grid, image: &Image<'_>, cx: i32, cy: i32, radius: i32) -> i32 {
    let mut score: i32 = 0;
    for i in 0..radius * 2 {
        score += fitness_cell(qr, image, cx - radius + i, cy - radius);
        score += fitness_cell(qr, image, cx - radius, cy + radius - i);
        score += fitness_cell(qr, image, cx + radius, cy - radius + i);
        score += fitness_cell(qr, image, cx + radius - i, cy + radius);
    }

    score
}

fn fitness_apat(qr: &Grid, image: &Image<'_>, cx: i32, cy: i32) -> i32 {
    fitness_cell(qr, image, cx, cy) - fitness_ring(qr, image, cx, cy, 1)
        + fitness_ring(qr, image, cx, cy, 2)
}

fn fitness_capstone(qr: &Grid, image: &Image<'_>, mut x: i32, mut y: i32) -> i32 {
    x += 3;
    y += 3;

    fitness_cell(qr, image, x, y) + fitness_ring(qr, image, x, y, 1)
        - fitness_ring(qr, image, x, y, 2)
        + fitness_ring(qr, image, x, y, 3)
}

/// Compute a fitness score for the currently configured perspective
/// transform, using the features we expect to find by scanning the
/// grid.
fn fitness_all(q: &Quirc, index: usize) -> i32 {
    let qr = &q.grids[index];
    let image = Image {
        pixels: &q.pixels,
        width: q.w,
        height: q.h,
    };

    let version = (qr.grid_size - 17) / 4;
    let info = &VERSION_DB[version as usize];
    let mut score: i32 = 0;

    /* Check the timing pattern */
    for i in 0..qr.grid_size - 14 {
        let expect = if i & 1 != 0 { 1 } else { -1 };
        score += fitness_cell(qr, &image, i + 7, 6) * expect;
        score += fitness_cell(qr, &image, 6, i + 7) * expect;
    }

    /* Check capstones */
    score += fitness_capstone(qr, &image, 0, 0);
    score += fitness_capstone(qr, &image, qr.grid_size - 7, 0);
    score += fitness_capstone(qr, &image, 0, qr.grid_size - 7);
    if version < 0 || version > 40 {
        return score;
    }

    /* Check alignment patterns */
    let mut ap_count = 0;
    while ap_count < 7 && info.apat[ap_count] != 0 {
        ap_count += 1;
    }

    for x in &info.apat[1..ap_count - 1] {
        score += fitness_apat(qr, &image, 6, *x);
        score += fitness_apat(qr, &image, *x, 6);
    }

    for x in &info.apat[1..ap_count] {
        for y in &info.apat[1..ap_count] {
            score += fitness_apat(qr, &image, *x, *y);
        }
    }

    score
}

unsafe fn jiggle_perspective(q: *mut Quirc, index: usize) {
    let mut qr = &mut (*q).grids[index];
    let mut best = fitness_all(&*q, index);
    let mut adjustments: [f64; 8] = [0.; 8];

    for (a_val, c_val) in adjustments.iter_mut().zip(qr.c.iter()) {
        *a_val = c_val * 0.02;
    }

    for _pass in 0..5 {
        for i in 0..16 {
            let j = i >> 1;
            let old = qr.c[j];
            let step = adjustments[j];
            qr.c[j] = if i & 1 != 0 { old + step } else { old - step };

            let test = fitness_all(&*q, index);
            if test > best {
                best = test
            } else {
                qr.c[j] = old
            }
        }

        for val in &mut adjustments {
            *val *= 0.5;
        }
    }
}

/// Once the capstones are in place and an alignment point has been
/// chosen, we call this function to set up a grid-reading perspective
/// transform.
unsafe fn setup_qr_perspective(q: *mut Quirc, index: usize) {
    let qr = &mut (*q).grids[index];

    /* Set up the perspective map for reading the grid */
    let rect = [
        (*q).capstones[qr.caps[1]].corners[0],
        (*q).capstones[qr.caps[2]].corners[0],
        qr.align,
        (*q).capstones[qr.caps[0]].corners[0],
    ];

    perspective_setup(
        &mut qr.c,
        &rect,
        (qr.grid_size - 7) as f64,
        (qr.grid_size - 7) as f64,
    );
    jiggle_perspective(q, index);
}

/// Rotate the capstone with so that corner 0 is the leftmost with respect
/// to the given reference line.
unsafe fn rotate_capstone(cap: *mut Capstone, h0: *const Point, hd: *const Point) {
    let mut copy: [Point; 4] = [Point::default(); 4];
    let mut best = 0;
    let mut best_score = 2147483647;

    for (j, p) in (*cap).corners.iter().enumerate() {
        let score = (p.x - (*h0).x) * -(*hd).y + (p.y - (*h0).y) * (*hd).x;
        if j == 0 || score < best_score {
            best = j;
            best_score = score
        }
    }

    /* Rotate the capstone */
    for (i, copy) in copy.iter_mut().enumerate() {
        *copy = (*cap).corners[(i + best) % 4];
    }

    (*cap).corners = copy;
    perspective_setup(&mut (*cap).c, &(*cap).corners, 7.0, 7.0);
}

unsafe fn record_qr_grid(q: *mut Quirc, mut a: usize, b: usize, mut c: usize) {
    if (*q).count() >= 8 {
        return;
    }
    /* Construct the hypotenuse line from A to C. B should be to
     * the left of this line.
     */
    let mut h0 = (*q).capstones[a as usize].center;
    let mut hd = Point {
        x: (*q).capstones[c].center.x - (*q).capstones[a].center.x,
        y: (*q).capstones[c].center.y - (*q).capstones[a].center.y,
    };

    /* Make sure A-B-C is clockwise */
    if ((*q).capstones[b].center.x - h0.x) * -hd.y + ((*q).capstones[b].center.y - h0.y) * hd.x > 0
    {
        let swap = a;
        a = c;
        c = swap;
        hd.x = -hd.x;
        hd.y = -hd.y
    }
    /* Record the grid and its components */
    let qr_index = (*q).count();

    let mut qr = Grid::default();
    qr.caps[0] = a;
    qr.caps[1] = b;
    qr.caps[2] = c;
    qr.align_region = -1;

    (*q).grids.push(qr);

    let qr = &mut (*q).grids[qr_index];

    /* Rotate each capstone so that corner 0 is top-left with respect
     * to the grid.
     */
    for cap_index in &qr.caps {
        let mut cap = &mut (*q).capstones[*cap_index];
        rotate_capstone(cap, &mut h0, &mut hd);
        cap.qr_grid = qr_index as i32;
    }

    /* Check the timing pattern. This doesn't require a perspective
     * transform.
     */
    if !(measure_timing_pattern(q, qr_index) < 0) {
        /* Make an estimate based for the alignment pattern based on extending
         * lines from capstones A and C.
         */
        if !(line_intersect(
            &mut (*q).capstones[a as usize].corners[0],
            &mut (*q).capstones[a as usize].corners[1],
            &mut (*q).capstones[c as usize].corners[0],
            &mut (*q).capstones[c as usize].corners[3],
            &mut qr.align,
        ) == 0)
        {
            /* On V2+ grids, we should use the alignment pattern. */
            if qr.grid_size > 21 {
                /* Try to find the actual location of the alignment pattern. */
                find_alignment_pattern(q, qr_index);
                /* Find the point of the alignment pattern closest to the
                 * top-left of the QR grid.
                 */
                if qr.align_region >= 0 {
                    let reg = &(*q).regions[qr.align_region as usize];

                    /* Start from some point inside the alignment pattern */
                    qr.align = reg.seed;

                    let mut corners = [
                        qr.align,
                        Point::default(),
                        Point::default(),
                        Point::default(),
                    ];
                    let mut psd = PolygonScoreData {
                        ref_0: hd,
                        scores: [0; 4],
                        corners: &mut corners,
                    };
                    psd.scores[0] = -hd.y * qr.align.x + hd.x * qr.align.y;
                    flood_fill_seed::<fn(UserData<'_>, i32, i32, i32) -> ()>(
                        q,
                        reg.seed.x,
                        reg.seed.y,
                        qr.align_region,
                        1,
                        None,
                        UserData::None,
                        0,
                    );
                    flood_fill_seed(
                        q,
                        reg.seed.x,
                        reg.seed.y,
                        1,
                        qr.align_region,
                        Some(&find_leftmost_to_line),
                        UserData::Polygon(Rc::new(RefCell::new(&mut psd))),
                        0,
                    );
                    qr.align = corners[0];
                }
            }
            setup_qr_perspective(q, qr_index);
            return;
        }
    }

    /* We've been unable to complete setup for this grid. Undo what we've
     * recorded and pretend it never happened.
     */
    for cap_index in &qr.caps {
        (*q).capstones[*cap_index].qr_grid = -1;
    }

    (*q).grids.pop();
}

unsafe fn test_neighbours(q: *mut Quirc, i: usize, hlist: &NeighbourList, vlist: &NeighbourList) {
    let mut best_score = 0.0;
    let mut best_h = -1;
    let mut best_v = -1;

    /* Test each possible grouping */
    for hn in &hlist.n[..hlist.count] {
        for vn in &vlist.n[0..vlist.count] {
            let score = (1.0 - hn.distance / vn.distance).abs();

            if score > 2.5 {
                continue;
            }

            if best_h < 0 || score < best_score {
                best_h = hn.index;
                best_v = vn.index;
                best_score = score
            }
        }
    }

    if best_h < 0 || best_v < 0 {
        return;
    }

    record_qr_grid(q, best_h as usize, i, best_v as usize);
}

unsafe fn test_grouping(q: *mut Quirc, i: usize) {
    let c1 = &mut (*q).capstones[i];
    let mut hlist = NeighbourList {
        n: [Neighbour {
            index: 0,
            distance: 0.,
        }; 32],
        count: 0,
    };
    let mut vlist = NeighbourList {
        n: [Neighbour {
            index: 0,
            distance: 0.,
        }; 32],
        count: 0,
    };
    if c1.qr_grid >= 0 {
        return;
    }
    hlist.count = 0;
    vlist.count = 0;
    /* Look for potential neighbours by examining the relative gradients
     * from this capstone to others.
     */
    for (j, c2) in (*q).capstones.iter_mut().enumerate() {
        let mut u = 0.;
        let mut v = 0.;

        if i as usize == j || c2.qr_grid >= 0 {
            continue;
        }

        perspective_unmap(&(*c1).c, &mut c2.center, &mut u, &mut v);
        u = (u - 3.5).abs();
        v = (v - 3.5).abs();

        if u < 0.2 * v {
            let count = hlist.count as usize;
            hlist.count += 1;
            let n = &mut hlist.n[count];
            n.index = j as i32;
            n.distance = v;
        }

        if v < 0.2 * u {
            let count = vlist.count as usize;
            vlist.count += 1;
            let n = &mut vlist.n[count];
            n.index = j as i32;
            n.distance = u;
        }
    }

    if !(hlist.count != 0 && vlist.count != 0) {
        return;
    }

    test_neighbours(q, i, &mut hlist, &mut vlist);
}

unsafe fn pixels_setup(q: *mut Quirc, threshold: u8) {
    let source = &(*q).image;
    let dest = &mut (*q).pixels;

    for (value, dest) in source.iter().zip(dest.iter_mut()) {
        *dest = if (*value as i32) < threshold as i32 {
            1
        } else {
            0
        } as Pixel;
    }
}
/// These functions are used to process images for QR-code recognition.
/// quirc_begin() must first be called to obtain access to a buffer into
/// which the input image should be placed. Optionally, the current
/// width and height may be returned.
pub unsafe fn quirc_begin(q: *mut Quirc, w: &mut usize, h: &mut usize) -> *mut u8 {
    let q = &mut *q;

    q.regions.push(Default::default());
    q.regions.push(Default::default());

    q.capstones.clear();
    q.grids.clear();

    *w = q.w;
    *h = q.h;

    q.image.as_mut_ptr()
}

/// After filling the buffer, quirc_end() should be called to process
/// the image for QR-code recognition. The locations and content of each
/// code may be obtained using accessor functions described below.
pub unsafe fn quirc_end(q: *mut Quirc) {
    let threshold = otsu(q);
    pixels_setup(q, threshold);

    for i in 0..(*q).h {
        finder_scan(q, i);
    }

    for i in 0..(*q).num_capstones() {
        test_grouping(q, i);
    }
}

/// Extract the QR-code specified by the given index.
pub unsafe fn quirc_extract(q: *const Quirc, index: usize, code: &mut Code) {
    let qr = &(*q).grids[index as usize];
    if index > (*q).count() {
        return;
    }
    code.clear();

    perspective_map(&qr.c, 0.0, 0.0, &mut (*code).corners[0]);
    perspective_map(&qr.c, qr.grid_size as f64, 0.0, &mut (*code).corners[1]);
    perspective_map(
        &qr.c,
        qr.grid_size as f64,
        qr.grid_size as f64,
        &mut (*code).corners[2],
    );
    perspective_map(&qr.c, 0.0, qr.grid_size as f64, &mut (*code).corners[3]);
    (*code).size = qr.grid_size;

    let mut i = 0;
    for y in 0..qr.grid_size {
        for x in 0..qr.grid_size {
            if read_cell(q, index, x, y) > 0 {
                (*code).cell_bitmap[(i >> 3) as usize] =
                    ((*code).cell_bitmap[(i >> 3) as usize] as i32 | 1 << (i & 7)) as u8
            }
            i += 1;
        }
    }
}
