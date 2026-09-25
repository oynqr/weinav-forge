use crate::{policy::System, record, seed::State};
use anyhow::{Context, Result, ensure};
use fearless_simd::{Level, dispatch, prelude::*};
use nalgebra::{DMatrix, DVector, Vector3};
use std::{
    collections::BTreeMap,
    f64::consts::{PI, TAU},
};

pub const PARAMETER_NAMES: [&str; 15] = [
    "sqrt_a", "ecc", "i0", "omega0", "omega", "m0", "delta_n", "omegadot", "idot", "cuc", "cus",
    "cic", "cis", "crc", "crs",
];
pub type Parameters = [f64; 15];

pub fn gravity(system: System) -> f64 {
    if matches!(system, System::Gps | System::Qzs) {
        3.986_005e14
    } else {
        3.986_004_418e14
    }
}

pub fn earth_rate(system: System) -> f64 {
    if system == System::Bds {
        7.292_115e-5
    } else {
        7.292_115_146_7e-5
    }
}

pub fn angle(value: f64) -> f64 {
    (value + PI).rem_euclid(TAU) - PI
}

pub fn position(p: &Parameters, dt: f64, toe: f64, system: System, geo: bool) -> Result<[f64; 3]> {
    ensure!(
        p.iter().all(|x| x.is_finite()) && p[0] > 0.0 && p[1].abs() < 1.0,
        "invalid Kepler elements"
    );
    let [
        sqa,
        ecc,
        i0,
        omega0,
        omega,
        m0,
        dn,
        omd,
        idot,
        cuc,
        cus,
        cic,
        cis,
        crc,
        crs,
    ] = *p;
    let a = sqa * sqa;
    let n = gravity(system).sqrt() / sqa.powi(3) + dn;
    let mean = angle(m0 + n * dt);
    let mut eccentric = mean;
    for _ in 0..20 {
        let delta = (eccentric - ecc * eccentric.sin() - mean) / (1.0 - ecc * eccentric.cos());
        eccentric -= delta;
        if delta.abs() < 1e-14 {
            break;
        }
    }
    let v = ((1.0 - ecc * ecc).sqrt() * eccentric.sin()).atan2(eccentric.cos() - ecc);
    let phi = v + omega;
    let (s2, c2) = (2.0 * phi).sin_cos();
    let u = phi + cuc * c2 + cus * s2;
    let r = a * (1.0 - ecc * eccentric.cos()) + crc * c2 + crs * s2;
    let inc = i0 + idot * dt + cic * c2 + cis * s2;
    let rate = earth_rate(system);
    let node = omega0 + (omd - if geo { 0.0 } else { rate }) * dt - rate * toe;
    let xp = r * u.cos();
    let yp = r * u.sin();
    let x = xp * node.cos() - yp * inc.cos() * node.sin();
    let y = xp * node.sin() + yp * inc.cos() * node.cos();
    let z = yp * inc.sin();
    if geo {
        let (s, c) = (-5_f64.to_radians()).sin_cos();
        let y1 = y * c + z * s;
        let z1 = -y * s + z * c;
        let (s, c) = (rate * dt).sin_cos();
        Ok([x * c + y1 * s, -x * s + y1 * c, z1])
    } else {
        Ok([x, y, z])
    }
}

pub fn initial(state: &State, toe: f64, system: System) -> Result<Parameters> {
    let r = Vector3::from(state.position);
    let v = Vector3::from(state.velocity) + Vector3::new(0.0, 0.0, earth_rate(system)).cross(&r);
    let radius = r.norm();
    let h = r.cross(&v);
    let gm = gravity(system);
    let a = -gm / (v.dot(&v) - 2.0 * gm / radius);
    let eccentricity = v.cross(&h) / gm - r / radius;
    let ecc = eccentricity.norm();
    ensure!(
        a > 0.0 && ecc < 0.5 && h.norm() > 0.0,
        "cannot initialise orbit from state"
    );
    let inc = (h.z / h.norm()).clamp(-1.0, 1.0).acos();
    let node = Vector3::new(-h.y, h.x, 0.0);
    let raan = node.y.atan2(node.x);
    let mut omega = if node.norm() * ecc > 1e-12 {
        (node.dot(&eccentricity) / (node.norm() * ecc))
            .clamp(-1.0, 1.0)
            .acos()
    } else {
        0.0
    };
    if eccentricity.z < 0.0 {
        omega = -omega;
    }
    let mut anomaly = if ecc > 1e-12 {
        (eccentricity.dot(&r) / (ecc * radius))
            .clamp(-1.0, 1.0)
            .acos()
    } else {
        r.y.atan2(r.x) - raan
    };
    if r.dot(&v) < 0.0 {
        anomaly = -anomaly;
    }
    let eccentric = 2.0 * ((anomaly / 2.0).tan() * (1.0 - ecc).sqrt()).atan2((1.0 + ecc).sqrt());
    Ok([
        a.sqrt(),
        ecc,
        inc,
        raan + earth_rate(system) * toe,
        omega,
        eccentric - ecc * eccentric.sin(),
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
        0.0,
    ])
}

fn to_regular(p: Parameters) -> Parameters {
    let mut u = p;
    u[1] = p[1] * p[4].cos();
    u[4] = p[1] * p[4].sin();
    u[5] = p[4] + p[5];
    u
}

fn from_regular(u: Parameters) -> Parameters {
    let mut p = u;
    p[1] = u[1].hypot(u[4]);
    p[4] = u[4].atan2(u[1]);
    p[5] = u[5] - p[4];
    p
}

fn jacobian(
    samples: &[(f64, [f64; 3])],
    toe: f64,
    system: System,
    u: Parameters,
    indices: &[usize],
    level: Level,
) -> DMatrix<f64> {
    dispatch!(level, simd => jacobian_simd(simd, samples, toe, system, u, indices))
}

#[inline(always)]
fn jacobian_simd<S: Simd>(
    simd: S,
    samples: &[(f64, [f64; 3])],
    toe: f64,
    system: System,
    u: Parameters,
    indices: &[usize],
) -> DMatrix<f64> {
    let [
        sqa,
        ecc,
        i0,
        omega0,
        omega,
        m0,
        dn,
        omd,
        idot,
        cuc,
        cus,
        cic,
        cis,
        crc,
        crs,
    ] = from_regular(u);
    let a = sqa * sqa;
    let unperturbed_motion = gravity(system).sqrt() / sqa.powi(3);
    let n = unperturbed_motion + dn;
    let eccentricity_scale = (1.0 - ecc * ecc).sqrt();
    let rate = earth_rate(system);
    const PADDED_PARAMETERS: usize = PARAMETER_NAMES.len().next_power_of_two();
    let mut j = DMatrix::zeros(samples.len() * 3, indices.len());
    for (sample_index, &(dt, _)) in samples.iter().enumerate() {
        let mean = angle(m0 + n * dt);
        let mut eccentric = mean;
        for _ in 0..20 {
            let (sin_e, cos_e) = eccentric.sin_cos();
            let delta = (eccentric - ecc * sin_e - mean) / (1.0 - ecc * cos_e);
            eccentric -= delta;
            if delta.abs() < 1e-14 {
                break;
            }
        }
        let (sin_e, cos_e) = eccentric.sin_cos();
        let denominator = 1.0 - ecc * cos_e;
        let anomaly = (eccentricity_scale * sin_e).atan2(cos_e - ecc);
        let phi = anomaly + omega;
        let (s2, c2) = (2.0 * phi).sin_cos();
        let argument = phi + cuc * c2 + cus * s2;
        let radius = a * denominator + crc * c2 + crs * s2;
        let inclination = i0 + idot * dt + cic * c2 + cis * s2;
        let node = omega0 + (omd - rate) * dt - rate * toe;
        let (sin_u, cos_u) = argument.sin_cos();
        let (sin_i, cos_i) = inclination.sin_cos();
        let (sin_node, cos_node) = node.sin_cos();
        let xp = radius * cos_u;
        let yp = radius * sin_u;
        let x = xp * cos_node - yp * cos_i * sin_node;
        let y = xp * sin_node + yp * cos_i * cos_node;
        let mut derivatives = [[0.0; PADDED_PARAMETERS]; 3];
        for offset in (0..PADDED_PARAMETERS).step_by(S::f64s::LEN) {
            let d_mean = S::f64s::from_fn(simd, |lane| match offset + lane {
                0 => -3.0 * unperturbed_motion * dt / sqa,
                5 => 1.0,
                6 => dt,
                _ => 0.0,
            });
            let d_ecc = S::f64s::from_fn(simd, |lane| if offset + lane == 1 { 1.0 } else { 0.0 });
            let d_omega = S::f64s::from_fn(simd, |lane| if offset + lane == 4 { 1.0 } else { 0.0 });
            let d_e = (d_mean + d_ecc * sin_e) / denominator;
            let d_phi = d_e * (eccentricity_scale / denominator)
                + d_ecc * (sin_e / (eccentricity_scale * denominator))
                + d_omega;
            let d_argument = d_phi * (1.0 - 2.0 * cuc * s2 + 2.0 * cus * c2)
                + S::f64s::from_fn(simd, |lane| match offset + lane {
                    9 => c2,
                    10 => s2,
                    _ => 0.0,
                });
            let d_radius = (d_e * (ecc * sin_e) - d_ecc * cos_e) * a
                + d_phi * (2.0 * (-crc * s2 + crs * c2))
                + S::f64s::from_fn(simd, |lane| match offset + lane {
                    0 => 2.0 * sqa * denominator,
                    13 => c2,
                    14 => s2,
                    _ => 0.0,
                });
            let d_inclination = d_phi * (2.0 * (-cic * s2 + cis * c2))
                + S::f64s::from_fn(simd, |lane| match offset + lane {
                    2 => 1.0,
                    8 => dt,
                    11 => c2,
                    12 => s2,
                    _ => 0.0,
                });
            let d_node = S::f64s::from_fn(simd, |lane| match offset + lane {
                3 => 1.0,
                7 => dt,
                _ => 0.0,
            });
            let d_xp = d_radius * cos_u - d_argument * yp;
            let d_yp = d_radius * sin_u + d_argument * xp;
            let xyz = [
                d_xp * cos_node - d_yp * cos_i * sin_node + d_inclination * (yp * sin_i * sin_node)
                    - d_node * y,
                d_xp * sin_node + d_yp * cos_i * cos_node - d_inclination * (yp * sin_i * cos_node)
                    + d_node * x,
                d_yp * sin_i + d_inclination * (yp * cos_i),
            ];
            for (axis, value) in xyz.iter().enumerate() {
                value.store_slice(&mut derivatives[axis][offset..offset + S::f64s::LEN]);
            }
        }
        for (column, &index) in indices.iter().enumerate() {
            for axis in 0..3 {
                let derivative = match index {
                    1 => {
                        derivatives[axis][1] * u[1] / ecc
                            - (derivatives[axis][4] - derivatives[axis][5]) * u[4] / (ecc * ecc)
                    }
                    4 => {
                        derivatives[axis][1] * u[4] / ecc
                            + (derivatives[axis][4] - derivatives[axis][5]) * u[1] / (ecc * ecc)
                    }
                    _ => derivatives[axis][index],
                };
                j[(sample_index * 3 + axis, column)] = -derivative;
            }
        }
    }
    j
}

pub struct Fit {
    pub parameters: Parameters,
    pub rms: f64,
    pub iterations: usize,
}

pub fn fit(
    samples: &[(f64, [f64; 3])],
    toe: f64,
    system: System,
    start: Parameters,
    pinned_dn: Option<f64>,
) -> Result<Fit> {
    ensure!(samples.len() >= 9, "not enough samples to fit orbit");
    let indices: Vec<usize> = (0..15).filter(|&j| j != 6 || pinned_dn.is_none()).collect();
    let steps: [f64; 15] = [
        1e-5, 1e-9, 1e-9, 1e-9, 1e-9, 1e-9, 1e-13, 1e-13, 1e-13, 1e-9, 1e-9, 1e-9, 1e-9, 1e-4, 1e-4,
    ];
    let residual = |u: Parameters| -> Result<DVector<f64>> {
        let p = from_regular(u);
        ensure!(p[1] < 0.5, "eccentricity exceeds fit bound");
        let mut values = Vec::with_capacity(samples.len() * 3);
        for &(dt, xyz) in samples {
            let model = position(&p, dt, toe, system, false)?;
            values.extend((0..3).map(|i| xyz[i] - model[i]));
        }
        Ok(DVector::from_vec(values))
    };
    let mut u = to_regular(start);
    if let Some(dn) = pinned_dn {
        u[6] = dn;
    }
    let mut r = residual(u)?;
    let mut score = r.norm_squared();
    let mut damping = 1e-3;
    let mut iterations = 0;
    let simd_level = Level::new();
    for iteration in 0..80 {
        iterations = iteration + 1;
        let minimum_analytic_eccentricity = 1e-6;
        let mut j = if u[1].hypot(u[4]) >= minimum_analytic_eccentricity {
            jacobian(samples, toe, system, u, &indices, simd_level)
        } else {
            let mut j = DMatrix::zeros(r.len(), indices.len());
            for (column, &index) in indices.iter().enumerate() {
                let h = steps[index].max(u[index].abs() * 1e-7);
                let mut a = u;
                a[index] += h;
                let mut b = u;
                b[index] -= h;
                let derivative = (residual(a)? - residual(b)?) / (2.0 * h);
                j.set_column(column, &derivative);
            }
            j
        };
        let norms: Vec<f64> = (0..j.ncols())
            .map(|i| j.column(i).norm().max(1e-30))
            .collect();
        for (i, &norm) in norms.iter().enumerate() {
            j.column_mut(i).scale_mut(1.0 / norm);
        }
        let normal = j.transpose() * &j;
        let gradient = j.transpose() * &r;
        let mut improvement = false;
        let before = score;
        for _ in 0..20 {
            let mut damped = normal.clone();
            for i in 0..indices.len() {
                damped[(i, i)] += damping;
            }
            let Some(delta) = damped.lu().solve(&(-&gradient)) else {
                damping *= 10.0;
                continue;
            };
            let mut next = u;
            for (i, &index) in indices.iter().enumerate() {
                next[index] += delta[i] / norms[i];
            }
            if let Ok(candidate) = residual(next) {
                let next_score = candidate.norm_squared();
                if next_score < score {
                    u = next;
                    r = candidate;
                    score = next_score;
                    damping = (damping * 0.3).max(1e-12);
                    improvement = true;
                    break;
                }
            }
            damping *= 10.0;
        }
        if !improvement
            || (score / samples.len() as f64).sqrt() < 1e-4
            || before - score < before * 1e-12
        {
            break;
        }
    }
    Ok(Fit {
        parameters: from_regular(u),
        rms: (score / samples.len() as f64).sqrt(),
        iterations,
    })
}

pub fn record_values(
    system: System,
    svid: u8,
    time: f64,
    p: &Parameters,
    clock: [f64; 3],
    tgd: [f64; 2],
) -> BTreeMap<String, f64> {
    let mut values = record::zero_values(system);
    for (i, name) in PARAMETER_NAMES.iter().enumerate() {
        let angular = matches!(i, 2..=8);
        let value = if matches!(i, 3..=5) {
            angle(p[i])
        } else {
            p[i]
        };
        values.insert((*name).into(), if angular { value / PI } else { value });
    }
    let tow = (time - if system == System::Bds { 14.0 } else { 0.0 }).rem_euclid(604_800.0);
    values.insert("sv".into(), f64::from(svid - 1));
    values.insert("toe".into(), tow);
    values.insert("toc".into(), tow);
    values.insert("af0".into(), clock[0]);
    values.insert("af1".into(), clock[1]);
    match system {
        System::Gps => {
            values.insert("week".into(), (time / 604_800.0).floor());
            values.insert("unk4c".into(), 65280.0);
            values.insert("unk3e".into(), if clock[1] < 0.0 { -1.0 } else { 0.0 });
            values.insert("tgd".into(), tgd[0]);
        }
        System::Bds => {
            values.insert("toe_tag".into(), ((tow as u32 / 8) & 0xffc0) as f64);
            values.insert("af2".into(), clock[2]);
            values.insert("tgd1".into(), tgd[0]);
            values.insert("tgd2".into(), tgd[1]);
        }
        _ => {
            values.insert("tgd".into(), tgd[0]);
        }
    }
    values
}

pub fn parameters(values: &BTreeMap<String, f64>) -> Result<Parameters> {
    let mut p = [0.0; 15];
    for (i, name) in PARAMETER_NAMES.iter().enumerate() {
        p[i] = *values
            .get(*name)
            .with_context(|| format!("missing orbital field {name}"))?;
        if matches!(i, 2..=8) {
            p[i] *= PI;
        }
    }
    Ok(p)
}

pub fn glonass_acceleration(p: [f64; 3], v: [f64; 3]) -> [f64; 3] {
    let r2 = p.iter().map(|x| x * x).sum::<f64>();
    let r = r2.sqrt();
    let mu = 3.986_004_4e14;
    let j2 = 1.082_625_7e-3;
    let equator = 6_378_136.0;
    let rate = 7.292_115e-5;
    let central = -mu / (r2 * r);
    let perturbation = -1.5 * j2 * mu * equator * equator / (r2 * r2 * r);
    let z = 5.0 * p[2] * p[2] / r2;
    [
        central * p[0] + perturbation * p[0] * (1.0 - z) + rate * rate * p[0] + 2.0 * rate * v[1],
        central * p[1] + perturbation * p[1] * (1.0 - z) + rate * rate * p[1] - 2.0 * rate * v[0],
        central * p[2] + perturbation * p[2] * (3.0 - z),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simd_levels() -> Vec<Level> {
        let detected = Level::new();
        let mut levels = vec![Level::baseline()];
        levels.push(detected);
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        levels.extend(
            [
                detected.as_sse2().map(Level::Sse2),
                detected.as_sse4_2().map(Level::Sse4_2),
                detected.as_avx2().map(Level::Avx2),
                detected.as_avx512().map(Level::Avx512),
            ]
            .into_iter()
            .flatten(),
        );
        levels
    }

    #[test]
    fn analytic_jacobian_matches_central_differences() -> Result<()> {
        let indices: Vec<_> = (0..15).collect();
        let samples: Vec<_> = [-7200.0, -3600.0, 0.0, 3600.0, 7200.0]
            .into_iter()
            .map(|dt| (dt, [0.0; 3]))
            .collect();
        let steps: Parameters = [
            1e-3, 1e-8, 1e-7, 1e-7, 1e-8, 1e-7, 1e-11, 1e-11, 1e-11, 1e-7, 1e-7, 1e-7, 1e-7, 1e-2,
            1e-2,
        ];
        let toe = 597_600.0;
        for system in [System::Gps, System::Galileo, System::Bds, System::Qzs] {
            for ecc in [1e-6, 0.00035, 0.1, 0.49] {
                let p = [
                    5282.61, ecc, 0.96, 1.2, -0.7, 0.4, 2.1e-9, -2.3e-9, 3e-10, 1.4e-6, -2.2e-6,
                    8e-8, -6e-8, 210.0, -140.0,
                ];
                let u = to_regular(p);
                let analytic = jacobian(&samples, toe, system, u, &indices, Level::new());
                let pinned_indices: Vec<_> = (0..15).filter(|&i| i != 6).collect();
                for level in simd_levels() {
                    assert_eq!(
                        jacobian(&samples, toe, system, u, &indices, level),
                        analytic,
                        "{level:?}: Jacobian differs"
                    );
                    assert_eq!(
                        jacobian(&samples, toe, system, u, &pinned_indices, level),
                        analytic.clone().remove_column(6),
                        "{level:?}: pinned Jacobian differs"
                    );
                }
                for (column, &h) in steps.iter().enumerate() {
                    let mut above = u;
                    let mut below = u;
                    above[column] += h;
                    below[column] -= h;
                    let mut numeric = Vec::new();
                    for &(dt, _) in &samples {
                        let a = position(&from_regular(above), dt, toe, system, false)?;
                        let b = position(&from_regular(below), dt, toe, system, false)?;
                        numeric.extend((0..3).map(|axis| (b[axis] - a[axis]) / (2.0 * h)));
                    }
                    let numeric = DVector::from_vec(numeric);
                    let error = (analytic.column(column) - &numeric).norm() / numeric.norm();
                    assert!(
                        error < 1e-5,
                        "{system:?}, eccentricity {ecc}, {}: relative error {error}",
                        PARAMETER_NAMES[column]
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn fits_circular_orbits_and_preserves_pinned_mean_motion() -> Result<()> {
        for ecc in [0.0, 1e-8, 1e-6, 0.1] {
            for pinned in [None, Some(2.1e-9)] {
                let p = [
                    6493.0, ecc, 0.7, 1.2, -0.7, 0.4, 2.1e-9, -2.3e-9, 3e-10, 1.4e-6, -2.2e-6,
                    8e-8, -6e-8, 210.0, -140.0,
                ];
                let toe = 230_400.0;
                let samples = (-24..=24)
                    .map(|i| {
                        let dt = f64::from(i) * 300.0;
                        Ok((dt, position(&p, dt, toe, System::Qzs, false)?))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let mut start = p;
                start[0] += 0.01;
                start[5] += 1e-5;
                let fitted = fit(&samples, toe, System::Qzs, start, pinned)?;
                assert!(fitted.rms < 0.01, "eccentricity {ecc}: {}", fitted.rms);
                if let Some(dn) = pinned {
                    assert_eq!(fitted.parameters[6], dn);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn recovers_a_known_orbit_including_near_circular_eccentricity() -> Result<()> {
        for system in [System::Gps, System::Galileo, System::Bds, System::Qzs] {
            let p = [
                5282.61, 0.00035, 0.96, 1.2, -0.7, 0.4, 2.1e-9, -2.3e-9, 3e-10, 1.4e-6, -2.2e-6,
                8e-8, -6e-8, 210.0, -140.0,
            ];
            let toe = 230_400.0;
            let samples: Vec<_> = (-24..=24)
                .map(|i| {
                    let dt = f64::from(i) * 150.0;
                    Ok((dt, position(&p, dt, toe, system, false)?))
                })
                .collect::<Result<_>>()?;
            let left = position(&p, -0.5, toe, system, false)?;
            let right = position(&p, 0.5, toe, system, false)?;
            let state = State {
                position: position(&p, 0.0, toe, system, false)?,
                velocity: std::array::from_fn(|i| right[i] - left[i]),
                acceleration: [0.0; 3],
                clock: 0.0,
                drift: 0.0,
            };
            let fit = fit(&samples, toe, system, initial(&state, toe, system)?, None)?;
            assert!(fit.rms < 0.01, "{system:?}: RMS {}", fit.rms);
            assert!(fit.parameters[1] >= 0.0 && fit.parameters[1] < 1.0);
            for dt in [-3525.0, -1725.0, 75.0, 1875.0, 3525.0] {
                let expected = Vector3::from(position(&p, dt, toe, system, false)?);
                let actual = Vector3::from(position(&fit.parameters, dt, toe, system, false)?);
                assert!((actual - expected).norm() < 0.01);
            }
        }
        Ok(())
    }
}
