//! The Irregular Terrain Model (Longley-Rice), point-to-point mode, ported from NTIA's
//! reference implementation (github.com/NTIA/itm, the `ITM_P2P_TLS` path). Names follow
//! that code so the two can be read side by side; the tests are NTIA's own example cases.

use std::f64::consts::PI;

use anyhow::{Result, bail};

const A_0: f64 = 6370e3;
const A_9000: f64 = 9000e3;
const THIRD: f64 = 1.0 / 3.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarization {
    Horizontal,
    Vertical,
}

/// Radio climate, as numbered in ITM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Climate {
    Equatorial = 1,
    ContinentalSubtropical = 2,
    MaritimeSubtropical = 3,
    Desert = 4,
    ContinentalTemperate = 5,
    MaritimeTemperateOverLand = 6,
    MaritimeTemperateOverSea = 7,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub climate: Climate,
    /// surface refractivity, N-units
    pub n_0: f64,
    pub f_mhz: f64,
    pub pol: Polarization,
    /// relative permittivity of the ground
    pub epsilon: f64,
    /// ground conductivity, S/m
    pub sigma: f64,
    /// ITM's variability mode code (12 = mobile, no location variability)
    pub mdvar: i32,
    /// time, location and situation percentages
    pub time: f64,
    pub location: f64,
    pub situation: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    LineOfSight,
    Diffraction,
    Troposcatter,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::LineOfSight => "within the radio horizon",
            Mode::Diffraction => "beyond the radio horizon (diffraction)",
            Mode::Troposcatter => "far beyond the radio horizon (troposcatter)",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Loss {
    /// basic transmission loss, dB
    pub db: f64,
    pub mode: Mode,
}

fn fdim(a: f64, b: f64) -> f64 {
    if a > b { a - b } else { 0.0 }
}

/// A terrain profile: `np` intervals of `xi` metres, then np + 1 elevations, as ITM's
/// `pfl` array.
pub fn pfl(xi: f64, elevations: &[f64]) -> Vec<f64> {
    let mut v = Vec::with_capacity(elevations.len() + 2);
    v.push((elevations.len() - 1) as f64);
    v.push(xi);
    v.extend_from_slice(elevations);
    v
}

/// Basic transmission loss between two antennas `h_tx` and `h_rx` metres above the ends
/// of the profile.
pub fn p2p(h_tx: f64, h_rx: f64, pfl: &[f64], p: &Params) -> Result<Loss> {
    let np = pfl[0] as usize;
    if np < 2 || pfl.len() != np + 3 || pfl[1] <= 0.0 {
        bail!("a terrain profile needs at least 3 points at a positive spacing");
    }
    if !(0.5..=3000.0).contains(&h_tx) || !(0.5..=3000.0).contains(&h_rx) {
        bail!("antenna heights must be between 0.5 and 3000 m above ground");
    }
    if !(20.0..=20000.0).contains(&p.f_mhz) {
        bail!("ITM covers 20 MHz to 20 GHz");
    }
    if !(250.0..=400.0).contains(&p.n_0) || p.epsilon < 1.0 || p.sigma <= 0.0 {
        bail!("refractivity must be 250 to 400 N-units and the ground constants positive");
    }
    for v in [p.time, p.location, p.situation] {
        if !(0.0..100.0).contains(&v) || v <= 0.0 {
            bail!("time, location and situation must be between 0 and 100 percent");
        }
    }

    // average path height, ignoring the first and last 10%
    let p10 = (0.1 * np as f64) as usize;
    let h_sys: f64 = (p10..=np - p10).map(|i| pfl[i + 2]).sum::<f64>() / (np - 2 * p10 + 1) as f64;

    let (z_g, gamma_e, n_s) = initialize_p2p(p.f_mhz, h_sys, p.n_0, p.pol, p.epsilon, p.sigma);
    let h = [h_tx, h_rx];
    let q = quick_pfl(pfl, gamma_e, h);
    let (a_ref, mode) = longley_rice(&q, p.f_mhz, z_g, gamma_e, n_s, h)?;
    let a_fs = free_space_loss(q.d, p.f_mhz);
    let v = variability(p, &q, a_ref);
    Ok(Loss { db: v + a_fs, mode })
}

#[derive(Clone, Copy, Debug)]
struct C {
    re: f64,
    im: f64,
}

impl C {
    fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
    fn div(self, o: C) -> C {
        let d = o.re * o.re + o.im * o.im;
        C::new(
            (self.re * o.re + self.im * o.im) / d,
            (self.im * o.re - self.re * o.im) / d,
        )
    }
    fn scale(self, k: f64) -> C {
        C::new(self.re * k, self.im * k)
    }
    fn abs(self) -> f64 {
        self.re.hypot(self.im)
    }
    /// principal square root, as std::sqrt for complex<double>
    fn sqrt(self) -> C {
        let r = self.abs();
        let re = ((r + self.re) / 2.0).sqrt();
        let im = ((r - self.re) / 2.0).sqrt();
        C::new(re, if self.im < 0.0 { -im } else { im })
    }
}

fn initialize_p2p(
    f_mhz: f64,
    h_sys: f64,
    n_0: f64,
    pol: Polarization,
    epsilon: f64,
    sigma: f64,
) -> (C, f64, f64) {
    const GAMMA_A: f64 = 157e-9;
    let n_s = if h_sys == 0.0 {
        n_0
    } else {
        n_0 * (-h_sys / 9460.0).exp()
    };
    let gamma_e = GAMMA_A * (1.0 - 0.04665 * (n_s / 179.3).exp());
    let ep_r = C::new(epsilon, 18000.0 * sigma / f_mhz);
    let mut z_g = ep_r.sub(C::new(1.0, 0.0)).sqrt();
    if pol == Polarization::Vertical {
        z_g = z_g.div(ep_r);
    }
    (z_g, gamma_e, n_s)
}

struct Quick {
    theta_hzn: [f64; 2],
    d_hzn: [f64; 2],
    h_e: [f64; 2],
    delta_h: f64,
    d: f64,
}

fn find_horizons(pfl: &[f64], a_e: f64, h: [f64; 2]) -> ([f64; 2], [f64; 2]) {
    let np = pfl[0] as usize;
    let xi = pfl[1];
    let d = pfl[0] * pfl[1];
    let z_tx = pfl[2] + h[0];
    let z_rx = pfl[np + 2] + h[1];
    let mut theta = [
        (z_rx - z_tx) / d - d / (2.0 * a_e),
        -(z_rx - z_tx) / d - d / (2.0 * a_e),
    ];
    let mut d_hzn = [d, d];
    let mut d_tx = 0.0;
    let mut d_rx = d;
    for i in 1..np {
        d_tx += xi;
        d_rx -= xi;
        let theta_tx = (pfl[i + 2] - z_tx) / d_tx - d_tx / (2.0 * a_e);
        let theta_rx = -(z_rx - pfl[i + 2]) / d_rx - d_rx / (2.0 * a_e);
        if theta_tx > theta[0] {
            theta[0] = theta_tx;
            d_hzn[0] = d_tx;
        }
        if theta_rx > theta[1] {
            theta[1] = theta_rx;
            d_hzn[1] = d_rx;
        }
    }
    (theta, d_hzn)
}

fn linear_least_squares_fit(pfl: &[f64], d_start: f64, d_end: f64) -> (f64, f64) {
    let np = pfl[0] as i64;
    let mut i_start = fdim(d_start / pfl[1], 0.0) as i64;
    let mut i_end = np - fdim(np as f64, d_end / pfl[1]) as i64;
    if i_end <= i_start {
        i_start = fdim(i_start as f64, 1.0) as i64;
        i_end = np - fdim(np as f64, i_end as f64 + 1.0) as i64;
    }
    let x_length = (i_end - i_start) as f64;
    let mut mid = -0.5 * x_length;
    let mid_end = i_end as f64 + mid;
    let at = |i: i64| pfl[(i + 2) as usize];
    let mut sum_y = 0.5 * (at(i_start) + at(i_end));
    let mut scaled = 0.5 * (at(i_start) - at(i_end)) * mid;
    let mut i = 2.0;
    while i <= x_length {
        i_start += 1;
        mid += 1.0;
        sum_y += at(i_start);
        scaled += at(i_start) * mid;
        i += 1.0;
    }
    sum_y /= x_length;
    scaled = scaled * 12.0 / ((x_length * x_length + 2.0) * x_length);
    (
        sum_y - scaled * mid_end,
        sum_y + scaled * (np as f64 - mid_end),
    )
}

fn compute_delta_h(pfl: &[f64], d_start: f64, d_end: f64) -> f64 {
    let np = pfl[0] as i64;
    let mut x_start = d_start / pfl[1];
    let mut x_end = d_end / pfl[1];
    if x_end - x_start < 2.0 {
        return 0.0;
    }
    let p10 = ((0.1 * (x_end - x_start + 8.0)) as i64).clamp(4, 25);
    let n = 10 * p10 - 5;
    let p90 = n - p10;
    let np_s = (n - 1) as f64;
    let mut s = vec![0.0; n as usize + 2];
    s[0] = np_s;
    s[1] = 1.0;
    x_end = (x_end - x_start) / np_s;
    let mut i = x_start as i64;
    x_start -= (i as f32 + 1.0) as f64;
    for j in 0..n as usize {
        while x_start > 0.0 && (i + 1) < np {
            x_start -= 1.0;
            i += 1;
        }
        let k = i as usize;
        s[j + 2] = pfl[k + 3] + (pfl[k + 3] - pfl[k + 2]) * x_start;
        x_start += x_end;
    }
    let (mut fit_y1, fit_y2) = linear_least_squares_fit(&s, 0.0, np_s);
    let step = (fit_y2 - fit_y1) / np_s;
    let mut diffs: Vec<f64> = Vec::with_capacity(n as usize);
    for j in 0..n as usize {
        diffs.push(s[j + 2] - fit_y1);
        fit_y1 += step;
    }
    diffs.sort_by(|a, b| b.total_cmp(a));
    let q10 = diffs[(p10 - 1) as usize];
    let q90 = diffs[p90 as usize];
    (q10 - q90) / (1.0 - 0.8 * (-(d_end - d_start) / 50e3).exp())
}

fn quick_pfl(pfl: &[f64], gamma_e: f64, h: [f64; 2]) -> Quick {
    let d = pfl[0] * pfl[1];
    let np = pfl[0] as usize;
    let a_e = 1.0 / gamma_e;
    let (mut theta_hzn, mut d_hzn) = find_horizons(pfl, a_e, h);
    let d_start = (15.0 * h[0]).min(0.1 * d_hzn[0]);
    let d_end = d - (15.0 * h[1]).min(0.1 * d_hzn[1]);
    let delta_h = compute_delta_h(pfl, d_start, d_end);
    let mut h_e = [0.0; 2];
    let horizon =
        |h_e: f64| (2.0 * h_e * a_e).sqrt() * (-0.07 * (delta_h / h_e.max(5.0)).sqrt()).exp();
    if d_hzn[0] + d_hzn[1] > 1.5 * d {
        let (fit_tx, fit_rx) = linear_least_squares_fit(pfl, d_start, d_end);
        h_e[0] = h[0] + fdim(pfl[2], fit_tx);
        h_e[1] = h[1] + fdim(pfl[np + 2], fit_rx);
        for i in 0..2 {
            d_hzn[i] = horizon(h_e[i]);
        }
        let combined = d_hzn[0] + d_hzn[1];
        if combined <= d {
            let q = (d / combined).powi(2);
            for i in 0..2 {
                h_e[i] *= q;
                d_hzn[i] = horizon(h_e[i]);
            }
        }
        for i in 0..2 {
            let q = (2.0 * h_e[i] * a_e).sqrt();
            theta_hzn[i] = (0.65 * delta_h * (q / d_hzn[i] - 1.0) - 2.0 * h_e[i]) / q;
        }
    } else {
        let (fit_tx, _) = linear_least_squares_fit(pfl, d_start, 0.9 * d_hzn[0]);
        h_e[0] = h[0] + fdim(pfl[2], fit_tx);
        let (_, fit_rx) = linear_least_squares_fit(pfl, d - 0.9 * d_hzn[1], d_end);
        h_e[1] = h[1] + fdim(pfl[np + 2], fit_rx);
    }
    Quick {
        theta_hzn,
        d_hzn,
        h_e,
        delta_h,
        d,
    }
}

fn terrain_roughness(d: f64, delta_h: f64) -> f64 {
    delta_h * (1.0 - 0.8 * (-d / 50e3).exp())
}

fn sigma_h(delta_h: f64) -> f64 {
    0.78 * delta_h * (-0.5 * delta_h.powf(0.25)).exp()
}

fn fresnel_integral(v2: f64) -> f64 {
    if v2 < 5.76 {
        6.02 + 9.11 * v2.sqrt() - 1.27 * v2
    } else {
        12.953 + 10.0 * v2.log10()
    }
}

/// The path as ITM's loss functions see it.
struct Geo {
    f_mhz: f64,
    z_g: C,
    a_e: f64,
    n_s: f64,
    delta_h: f64,
    h: [f64; 2],
    h_e: [f64; 2],
    d_hzn: [f64; 2],
    theta_hzn: [f64; 2],
    theta_los: f64,
    d_sml: f64,
}

fn knife_edge(g: &Geo, d: f64) -> f64 {
    let d_ml = g.d_hzn[0] + g.d_hzn[1];
    let theta_nlos = d / g.a_e - g.theta_los;
    let d_nlos = d - d_ml;
    let v =
        |dh: f64| 0.0795775 * (g.f_mhz / 47.7) * theta_nlos.powi(2) * dh * d_nlos / (d_nlos + dh);
    fresnel_integral(v(g.d_hzn[0])) + fresnel_integral(v(g.d_hzn[1]))
}

fn height_function(x_km: f64, k: f64) -> f64 {
    if x_km < 200.0 {
        let w = -k.ln();
        if k < 1e-5 || x_km * w.powi(3) > 5495.0 {
            let mut r = -117.0;
            if x_km > 1.0 {
                r += 17.372 * x_km.ln();
            }
            r
        } else {
            2.5e-5 * x_km.powi(2) / k - 8.686 * w - 15.0
        }
    } else {
        let mut r = 0.05751 * x_km - 4.343 * x_km.ln();
        if x_km < 2000.0 {
            let w = 0.0134 * x_km * (-0.005 * x_km).exp();
            r = (1.0 - w) * r + w * (17.372 * x_km.ln() - 117.0);
        }
        r
    }
}

fn smooth_earth(g: &Geo, d: f64) -> f64 {
    let theta_nlos = d / g.a_e - g.theta_los;
    let d_ml = g.d_hzn[0] + g.d_hzn[1];
    let a = [
        (d - d_ml) / (d / g.a_e - g.theta_los),
        0.5 * g.d_hzn[0].powi(2) / g.h_e[0],
        0.5 * g.d_hzn[1].powi(2) / g.h_e[1],
    ];
    let d_km = [
        a[0] * theta_nlos / 1000.0,
        g.d_hzn[0] / 1000.0,
        g.d_hzn[1] / 1000.0,
    ];
    let mut c_0 = [0.0; 3];
    let mut k = [0.0; 3];
    let mut b_0 = [0.0; 3];
    for i in 0..3 {
        c_0[i] = ((4.0 / 3.0) * A_0 / a[i]).powf(THIRD);
        k[i] = 0.017778 * c_0[i] * g.f_mhz.powf(-THIRD) / g.z_g.abs();
        b_0[i] = 1.607 - k[i];
    }
    let f3 = g.f_mhz.powf(THIRD);
    let x1 = b_0[1] * c_0[1].powi(2) * f3 * d_km[1];
    let x2 = b_0[2] * c_0[2].powi(2) * f3 * d_km[2];
    let x0 = b_0[0] * c_0[0].powi(2) * f3 * d_km[0] + x1 + x2;
    let g_x = 0.05751 * x0 - 10.0 * x0.log10();
    g_x - height_function(x1, k[1]) - height_function(x2, k[2]) - 20.0
}

fn diffraction_loss(g: &Geo, d: f64) -> f64 {
    let a_k = knife_edge(g, d);
    let a_se = smooth_earth(g, d);
    let sigma_h_d = sigma_h(terrain_roughness(g.d_sml, g.delta_h));
    let a_fo = (5.0 * (1.0 + 1e-5 * g.h[0] * g.h[1] * g.f_mhz * sigma_h_d).log10()).min(15.0);
    let delta_h_d = terrain_roughness(d, g.delta_h);
    // point-to-point mode: low antennas with known path parameters, C ~= 10
    let q = g.h[0] * g.h[1] + 10.0;
    let qk = g.h_e[0] * g.h_e[1] - g.h[0] * g.h[1];
    let term1 = (1.0 + qk / q).sqrt();
    let d_ml = g.d_hzn[0] + g.d_hzn[1];
    let q = (term1 + (-g.theta_los * g.a_e + d_ml) / d) * (delta_h_d * g.f_mhz / 47.7).min(6283.2);
    let w = 25.1 / (25.1 + q.sqrt());
    w * a_se + (1.0 - w) * a_k + a_fo
}

fn line_of_sight_loss(g: &Geo, d: f64, m_d: f64, a_d0: f64) -> f64 {
    let sigma_h_d = sigma_h(terrain_roughness(d, g.delta_h));
    let wn = g.f_mhz / 47.7;
    let hh = g.h_e[0] + g.h_e[1];
    let sin_psi = hh / (d.powi(2) + hh.powi(2)).sqrt();
    let s = C::new(sin_psi, 0.0);
    let mut r_e = s
        .sub(g.z_g)
        .div(s.add(g.z_g))
        .scale((-(wn * sigma_h_d * sin_psi).min(10.0)).exp());
    let q = r_e.re.powi(2) + r_e.im.powi(2);
    if q < 0.25 || q < sin_psi {
        r_e = r_e.scale((sin_psi / q).sqrt());
    }
    let mut delta_phi = wn * 2.0 * g.h_e[0] * g.h_e[1] / d;
    if delta_phi > PI / 2.0 {
        delta_phi = PI - (PI / 2.0).powi(2) / delta_phi;
    }
    let rr = C::new(delta_phi.cos(), -delta_phi.sin()).add(r_e);
    let a_t = -10.0 * (rr.re.powi(2) + rr.im.powi(2)).log10();
    let a_d = m_d * d + a_d0;
    let w = 1.0 / (1.0 + g.f_mhz * g.delta_h / g.d_sml.max(10e3));
    w * a_t + (1.0 - w) * a_d
}

fn h0_curve(j: usize, r: f64) -> f64 {
    const A: [f64; 5] = [25.0, 80.0, 177.0, 395.0, 705.0];
    const B: [f64; 5] = [24.0, 45.0, 68.0, 80.0, 105.0];
    10.0 * (1.0 + A[j] * (1.0 / r).powi(4) + B[j] * (1.0 / r).powi(2)).log10()
}

fn h0_function(r: f64, eta_s: f64) -> f64 {
    let eta_s = eta_s.clamp(1.0, 5.0);
    let i = eta_s as usize;
    let q = eta_s - i as f64;
    let mut result = h0_curve(i - 1, r);
    if q != 0.0 {
        result = (1.0 - q) * result + q * h0_curve(i, r);
    }
    result
}

fn f_function(td: f64) -> f64 {
    const A: [f64; 3] = [133.4, 104.6, 71.8];
    const B: [f64; 3] = [0.332e-3, 0.212e-3, 0.157e-3];
    const C3: [f64; 3] = [-10.0, -2.5, 5.0];
    let i = if td <= 10e3 {
        0
    } else if td <= 70e3 {
        1
    } else {
        2
    };
    A[i] + B[i] * td + C3[i] * td.log10()
}

fn troposcatter_loss(g: &Geo, d: f64, h0: &mut f64) -> f64 {
    let (d_hzn, h_e, a_e, n_s) = (g.d_hzn, g.h_e, g.a_e, g.n_s);
    let wn = g.f_mhz / 47.7;
    let h_0 = if *h0 > 15.0 {
        *h0
    } else {
        let mut ad = d_hzn[0] - d_hzn[1];
        let mut rr = h_e[1] / h_e[0];
        if ad < 0.0 {
            ad = -ad;
            rr = 1.0 / rr;
        }
        let theta = g.theta_hzn[0] + g.theta_hzn[1] + d / a_e;
        let r_1 = 2.0 * wn * theta * h_e[0];
        let r_2 = 2.0 * wn * theta * h_e[1];
        if r_1 < 0.2 && r_2 < 0.2 {
            return 1001.0;
        }
        let mut s = (d - ad) / (d + ad);
        let q = (rr / s).clamp(0.1, 10.0);
        s = s.max(0.1);
        let h_0_m = (d - ad) * (d + ad) * theta * 0.25 / d;
        const Z_0: f64 = 1.7556e3;
        const Z_1: f64 = 8.0e3;
        let eta_s = (h_0_m / Z_0)
            * (1.0
                + (0.031 - n_s * 2.32e-3 + n_s.powi(2) * 5.67e-6)
                    * (-(h_0_m / Z_1).min(1.7).powi(6)).exp());
        let h_00 = (h0_function(r_1, eta_s) + h0_function(r_2, eta_s)) / 2.0;
        let delta_h_0 = h_00.min(6.0 * (0.6 - eta_s.max(1.0).log10()) * s.log10() * q.log10());
        let mut h_0 = (h_00 + delta_h_0).max(0.0);
        if eta_s < 1.0 {
            let sq2 = 2f64.sqrt();
            h_0 = eta_s * h_0
                + (1.0 - eta_s)
                    * 10.0
                    * (((1.0 + sq2 / r_1) * (1.0 + sq2 / r_2)).powi(2) * (r_1 + r_2)
                        / (r_1 + r_2 + 2.0 * sq2))
                        .log10();
        }
        if h_0 > 15.0 && *h0 >= 0.0 {
            h_0 = *h0;
        }
        h_0
    };
    *h0 = h_0;
    let th = d / a_e - g.theta_los;
    const D_0: f64 = 40e3;
    const H: f64 = 47.7;
    f_function(th * d) + 10.0 * (wn * H * th.powi(4)).log10()
        - 0.1 * (n_s - 301.0) * (-th * d / D_0).exp()
        + h_0
}

fn longley_rice(
    q: &Quick,
    f_mhz: f64,
    z_g: C,
    gamma_e: f64,
    n_s: f64,
    h: [f64; 2],
) -> Result<(f64, Mode)> {
    let (h_e, d_hzn, d) = (q.h_e, q.d_hzn, q.d);
    let a_e = 1.0 / gamma_e;
    let d_hzn_s = [(2.0 * h_e[0] * a_e).sqrt(), (2.0 * h_e[1] * a_e).sqrt()];
    let d_sml = d_hzn_s[0] + d_hzn_s[1];
    let d_ml = d_hzn[0] + d_hzn[1];
    let theta_los = -(q.theta_hzn[0] + q.theta_hzn[1]).max(-d_ml / a_e);
    if !(150.0..=400.0).contains(&n_s) {
        bail!("surface refractivity {n_s:.0} is outside ITM's range");
    }
    if !(4_000_000.0..=13_333_333.0).contains(&a_e) {
        bail!("effective earth radius is outside ITM's range");
    }
    if z_g.re <= z_g.im.abs() {
        bail!("ground impedance is outside ITM's range");
    }
    let g = Geo {
        f_mhz,
        z_g,
        a_e,
        n_s,
        delta_h: q.delta_h,
        h,
        h_e,
        d_hzn,
        theta_hzn: q.theta_hzn,
        theta_los,
        d_sml,
    };
    let scale = (a_e.powi(2) / f_mhz).powf(THIRD);
    let d_3 = d_sml.max(d_ml + 5.0 * scale);
    let d_4 = d_3 + 10.0 * scale;
    let a_3 = diffraction_loss(&g, d_3);
    let a_4 = diffraction_loss(&g, d_4);
    let m_d = (a_4 - a_3) / (d_4 - d_3);
    let a_d0 = a_3 - m_d * d_3;

    let (a_ref, mode) = if d < d_sml {
        let a_sml = d_sml * m_d + a_d0;
        let mut d_0 = 0.04 * f_mhz * h_e[0] * h_e[1];
        let d_1 = if a_d0 >= 0.0 {
            d_0 = d_0.min(0.5 * d_ml);
            d_0 + 0.25 * (d_ml - d_0)
        } else {
            (-a_d0 / m_d).max(0.25 * d_ml)
        };
        let los = |x: f64| line_of_sight_loss(&g, x, m_d, a_d0);
        let a_1 = los(d_1);
        let mut flag = false;
        let mut k1 = 0.0;
        let mut k2 = 0.0;
        if d_0 < d_1 {
            let a_0 = los(d_0);
            let q = (d_sml / d_0).ln();
            k2 = (((d_sml - d_0) * (a_1 - a_0) - (d_1 - d_0) * (a_sml - a_0))
                / ((d_sml - d_0) * (d_1 / d_0).ln() - (d_1 - d_0) * q))
                .max(0.0);
            flag = a_d0 > 0.0 || k2 > 0.0;
            if flag {
                k1 = (a_sml - a_0 - k2 * q) / (d_sml - d_0);
                if k1 < 0.0 {
                    k1 = 0.0;
                    k2 = fdim(a_sml, a_0) / q;
                    if k2 == 0.0 {
                        k1 = m_d;
                    }
                }
            }
        }
        if !flag {
            k1 = fdim(a_sml, a_1) / (d_sml - d_1);
            k2 = 0.0;
            if k1 == 0.0 {
                k1 = m_d;
            }
        }
        let a_o = a_sml - k1 * d_sml - k2 * d_sml.ln();
        (a_o + k1 * d + k2 * d.ln(), Mode::LineOfSight)
    } else {
        let d_5 = d_ml + 200e3;
        let d_6 = d_ml + 400e3;
        let mut h0 = -1.0;
        let a_6 = troposcatter_loss(&g, d_6, &mut h0);
        let a_5 = troposcatter_loss(&g, d_5, &mut h0);
        let (m_s, a_s0, d_x) = if a_5 < 1000.0 {
            let m_s = (a_6 - a_5) / 200e3;
            let d_x = d_sml
                .max(d_ml + 1.088 * scale * f_mhz.ln())
                .max((a_5 - a_d0 - m_s * d_5) / (m_d - m_s));
            (m_s, (m_d - m_s) * d_x + a_d0, d_x)
        } else {
            (m_d, a_d0, 10e6)
        };
        if d > d_x {
            (m_s * d + a_s0, Mode::Troposcatter)
        } else {
            (m_d * d + a_d0, Mode::Diffraction)
        }
    };
    Ok((a_ref.max(0.0), mode))
}

fn free_space_loss(d: f64, f_mhz: f64) -> f64 {
    32.45 + 20.0 * f_mhz.log10() + 20.0 * (d / 1000.0).log10()
}

fn inverse_ccdf(q: f64) -> f64 {
    const C_0: f64 = 2.515516;
    const C_1: f64 = 0.802853;
    const C_2: f64 = 0.010328;
    const D_1: f64 = 1.432788;
    const D_2: f64 = 0.189269;
    const D_3: f64 = 0.001308;
    let x = if q > 0.5 { 1.0 - q } else { q };
    let t = (-2.0 * x.ln()).sqrt();
    let zeta = ((C_2 * t + C_1) * t + C_0) / (((D_3 * t + D_2) * t + D_1) * t + 1.0);
    let r = t - zeta;
    if q > 0.5 { -r } else { r }
}

fn curve(c1: f64, c2: f64, x1: f64, x2: f64, x3: f64, d_e: f64) -> f64 {
    (c1 + c2 / (1.0 + ((d_e - x2) / x3).powi(2))) * (d_e / x1).powi(2) / (1.0 + (d_e / x1).powi(2))
}

fn variability(p: &Params, q: &Quick, a_ref: f64) -> f64 {
    let (h_e, delta_h, f_mhz, d) = (q.h_e, q.delta_h, p.f_mhz, q.d);
    const ALL_YEAR: [[f64; 7]; 5] = [
        [-9.67, -0.62, 1.26, -9.21, -0.62, -0.39, 3.15],
        [12.7, 9.19, 15.5, 9.05, 9.19, 2.86, 857.9],
        [
            144.9e3, 228.9e3, 262.6e3, 84.1e3, 228.9e3, 141.7e3, 2222.0e3,
        ],
        [
            190.3e3, 205.2e3, 185.2e3, 101.1e3, 205.2e3, 315.9e3, 164.8e3,
        ],
        [133.8e3, 143.6e3, 99.8e3, 98.6e3, 143.6e3, 167.4e3, 116.3e3],
    ];
    const BSM1: [f64; 7] = [2.13, 2.66, 6.11, 1.98, 2.68, 6.86, 8.51];
    const BSM2: [f64; 7] = [159.5, 7.67, 6.65, 13.11, 7.16, 10.38, 169.8];
    const XSM1: [f64; 7] = [762.2e3, 100.4e3, 138.2e3, 139.1e3, 93.7e3, 187.8e3, 609.8e3];
    const XSM2: [f64; 7] = [
        123.6e3, 172.5e3, 242.2e3, 132.7e3, 186.8e3, 169.6e3, 119.9e3,
    ];
    const XSM3: [f64; 7] = [94.5e3, 136.4e3, 178.6e3, 193.5e3, 133.5e3, 108.9e3, 106.6e3];
    const BSP1: [f64; 7] = [2.11, 6.87, 10.08, 3.68, 4.75, 8.58, 8.43];
    const BSP2: [f64; 7] = [102.3, 15.53, 9.60, 159.3, 8.12, 13.97, 8.19];
    const XSP1: [f64; 7] = [636.9e3, 138.7e3, 165.3e3, 464.4e3, 93.2e3, 216.0e3, 136.2e3];
    const XSP2: [f64; 7] = [134.8e3, 143.7e3, 225.7e3, 93.1e3, 135.9e3, 152.0e3, 188.5e3];
    const XSP3: [f64; 7] = [95.6e3, 98.6e3, 129.7e3, 94.2e3, 113.4e3, 122.7e3, 122.9e3];
    const C_D: [f64; 7] = [1.224, 0.801, 1.380, 1.000, 1.224, 1.518, 1.518];
    const Z_D: [f64; 7] = [1.282, 2.161, 1.282, 20.0, 1.282, 1.282, 1.282];
    const BFM1: [f64; 7] = [1.0, 1.0, 1.0, 1.0, 0.92, 1.0, 1.0];
    const BFM2: [f64; 7] = [0.0, 0.0, 0.0, 0.0, 0.25, 0.0, 0.0];
    const BFM3: [f64; 7] = [0.0, 0.0, 0.0, 0.0, 1.77, 0.0, 0.0];
    const BFP1: [f64; 7] = [1.0, 0.93, 1.0, 0.93, 0.93, 1.0, 1.0];
    const BFP2: [f64; 7] = [0.0, 0.31, 0.0, 0.19, 0.31, 0.0, 0.0];
    const BFP3: [f64; 7] = [0.0, 2.00, 0.0, 1.79, 2.00, 0.0, 0.0];
    const SINGLE: i32 = 0;
    const ACCIDENTAL: i32 = 1;
    const MOBILE: i32 = 2;

    let mut z_t = inverse_ccdf(p.time / 100.0);
    let mut z_l = inverse_ccdf(p.location / 100.0);
    let z_s = inverse_ccdf(p.situation / 100.0);
    let c = p.climate as usize - 1;
    let wn = f_mhz / 47.7;
    let d_ex = (2.0 * A_9000 * h_e[0]).sqrt()
        + (2.0 * A_9000 * h_e[1]).sqrt()
        + (575.7e12 / wn).powf(THIRD);
    let d_e = if d < d_ex {
        130e3 * d / d_ex
    } else {
        130e3 + d - d_ex
    };
    let mut md = p.mdvar;
    let plus20 = md >= 20;
    if plus20 {
        md -= 20;
    }
    let sigma_s = if plus20 {
        0.0
    } else {
        5.0 + 3.0 * (-d_e / 100e3).exp()
    };
    let plus10 = md >= 10;
    if plus10 {
        md -= 10;
    }
    let v_med = curve(
        ALL_YEAR[0][c],
        ALL_YEAR[1][c],
        ALL_YEAR[2][c],
        ALL_YEAR[3][c],
        ALL_YEAR[4][c],
        d_e,
    );
    if md == SINGLE {
        z_t = z_s;
        z_l = z_s;
    } else if md == ACCIDENTAL {
        z_l = z_s;
    } else if md == MOBILE {
        z_l = z_t;
    }
    let sigma_l = if plus10 {
        0.0
    } else {
        let dh = terrain_roughness(d, delta_h);
        10.0 * wn * dh / (wn * dh + 13.0)
    };
    let y_l = sigma_l * z_l;
    let q = (0.133 * wn).ln();
    let g_minus = BFM1[c] + BFM2[c] / ((BFM3[c] * q).powi(2) + 1.0);
    let g_plus = BFP1[c] + BFP2[c] / ((BFP3[c] * q).powi(2) + 1.0);
    let sigma_t_minus = curve(BSM1[c], BSM2[c], XSM1[c], XSM2[c], XSM3[c], d_e) * g_minus;
    let sigma_t_plus = curve(BSP1[c], BSP2[c], XSP1[c], XSP2[c], XSP3[c], d_e) * g_plus;
    let sigma_td = C_D[c] * sigma_t_plus;
    let tgtd = (sigma_t_plus - sigma_td) * Z_D[c];
    let sigma_t = if z_t < 0.0 {
        sigma_t_minus
    } else if z_t <= Z_D[c] {
        sigma_t_plus
    } else {
        sigma_td + tgtd / z_t
    };
    let y_t = sigma_t * z_t;
    let y_s_temp =
        sigma_s.powi(2) + y_t.powi(2) / (7.8 + z_s.powi(2)) + y_l.powi(2) / (24.0 + z_s.powi(2));
    let (y_r, y_s) = if md == SINGLE {
        (
            0.0,
            (sigma_t.powi(2) + sigma_l.powi(2) + y_s_temp).sqrt() * z_s,
        )
    } else if md == ACCIDENTAL {
        (y_t, (sigma_l.powi(2) + y_s_temp).sqrt() * z_s)
    } else if md == MOBILE {
        (
            (sigma_t.powi(2) + sigma_l.powi(2)).sqrt() * z_t,
            y_s_temp.sqrt() * z_s,
        )
    } else {
        (y_t + y_l, y_s_temp.sqrt() * z_s)
    };
    let mut result = a_ref - v_med - y_r - y_s;
    if result < 0.0 {
        result = result * (29.0 - result) / (29.0 - 10.0 * result);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse one line of NTIA's pfls.csv: np, xi, then np + 1 elevations.
    fn profile(line: &str) -> Vec<f64> {
        let v: Vec<f64> = line.split(',').map(|x| x.trim().parse().unwrap()).collect();
        assert_eq!(v.len(), v[0] as usize + 3, "profile length");
        v
    }

    fn case(
        eps_sigma: (f64, f64),
        n_0: f64,
        f_mhz: f64,
        pol: i32,
        climate: Climate,
        tls: (f64, f64, f64),
        mdvar: i32,
    ) -> Params {
        Params {
            climate,
            n_0,
            f_mhz,
            pol: if pol == 1 {
                Polarization::Vertical
            } else {
                Polarization::Horizontal
            },
            epsilon: eps_sigma.0,
            sigma: eps_sigma.1,
            mdvar,
            time: tls.0,
            location: tls.1,
            situation: tls.2,
        }
    }

    // NTIA's p2p.csv rows 2 to 5, with the matching pfls.csv profiles; row 1's profile
    // has 3680 points and isn't copied here
    const PFL_2: &str = include_str!("../tests/data/pfl2.csv");
    const PFL_3: &str = include_str!("../tests/data/pfl3.csv");
    const PFL_4: &str = include_str!("../tests/data/pfl4.csv");
    const PFL_5: &str = include_str!("../tests/data/pfl5.csv");

    #[test]
    fn matches_ntia_reference_cases() {
        let cases = [
            (
                PFL_2,
                case(
                    (15.0, 0.008),
                    301.0,
                    480.0,
                    1,
                    Climate::ContinentalTemperate,
                    (22.0, 22.0, 22.0),
                    12,
                ),
                (3.0, 1.5),
                157.10,
            ),
            (
                PFL_3,
                case(
                    (15.0, 0.008),
                    301.0,
                    990.0,
                    0,
                    Climate::Desert,
                    (15.0, 40.0, 50.0),
                    12,
                ),
                (15.0, 3.0),
                178.53,
            ),
            (
                PFL_4,
                case(
                    (15.0, 0.008),
                    301.0,
                    5600.0,
                    0,
                    Climate::ContinentalSubtropical,
                    (90.0, 30.0, 88.0),
                    12,
                ),
                (3.0, 5.0),
                183.26,
            ),
            (
                PFL_5,
                case(
                    (15.0, 0.008),
                    301.0,
                    8800.0,
                    1,
                    Climate::Equatorial,
                    (23.0, 95.0, 20.0),
                    12,
                ),
                (1.5, 10.0),
                218.91,
            ),
        ];
        for (pfl, p, (h_tx, h_rx), want) in cases {
            let got = p2p(h_tx, h_rx, &profile(pfl), &p).unwrap();
            assert!(
                (got.db - want).abs() < 0.01,
                "{} MHz: got {:.2} dB, NTIA has {want}",
                p.f_mhz,
                got.db
            );
        }
    }

    #[test]
    fn flat_short_path_is_line_of_sight_near_free_space() {
        let pfl = pfl(100.0, &[100.0; 51]);
        let p = Params {
            climate: Climate::ContinentalTemperate,
            n_0: 301.0,
            f_mhz: 5800.0,
            pol: Polarization::Vertical,
            epsilon: 15.0,
            sigma: 0.005,
            mdvar: 12,
            time: 50.0,
            location: 50.0,
            situation: 50.0,
        };
        let l = p2p(30.0, 30.0, &pfl, &p).unwrap();
        assert_eq!(l.mode, Mode::LineOfSight);
        let fs = free_space_loss(5000.0, 5800.0);
        assert!((l.db - fs).abs() < 10.0, "{} vs free space {fs}", l.db);
        // a ridge in the middle blocks it and costs a lot more
        let mut e = vec![100.0; 51];
        for x in &mut e[20..31] {
            *x = 400.0;
        }
        let blocked = p2p(30.0, 30.0, &super::pfl(100.0, &e), &p).unwrap();
        assert!(blocked.db > l.db + 20.0, "{} vs {}", blocked.db, l.db);
        // ITM picks its mode by distance against the smooth-earth horizon, not by what
        // blocks the path: 60 km over flat ground with 10 m masts is past that horizon
        let far = p2p(10.0, 10.0, &super::pfl(500.0, &[100.0; 121]), &p).unwrap();
        assert_ne!(far.mode, Mode::LineOfSight);
    }

    #[test]
    fn rejects_inputs_outside_its_range() {
        let p = Params {
            climate: Climate::ContinentalTemperate,
            n_0: 301.0,
            f_mhz: 50000.0,
            pol: Polarization::Vertical,
            epsilon: 15.0,
            sigma: 0.005,
            mdvar: 12,
            time: 50.0,
            location: 50.0,
            situation: 50.0,
        };
        assert!(p2p(10.0, 10.0, &pfl(100.0, &[0.0; 20]), &p).is_err());
    }
}
