// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Newton–Raphson with Levenberg–Marquardt damping over the residual system,
//! plus rank-based diagnostics (DOF count, redundant-constraint detection).

use crate::Sketch;

/// Residual magnitude below which a constraint counts as satisfied.
pub const SATISFIED_TOL: f64 = 1e-8;
/// Relative tolerance for rank decisions on the Jacobian.
const RANK_TOL: f64 = 1e-7;
const MAX_ITER: usize = 200;

/// Outcome of a [`Sketch::solve`] run.
#[derive(Debug, Clone, PartialEq)]
pub enum SolveStatus {
    /// All constraints satisfied.
    Converged,
    /// Could not satisfy every constraint (conflicting/inconsistent system).
    Inconsistent,
}

/// Solve diagnostics.
#[derive(Debug, Clone)]
pub struct SolveResult {
    pub status: SolveStatus,
    /// Remaining degrees of freedom (params − rank of the Jacobian). 3 for a
    /// fully-dimensioned floating rigid sketch (x, y, rotation); 0 when
    /// anchored and fully constrained.
    pub dof: usize,
    /// Indices (into the sketch's constraint list) of constraints whose
    /// equations are linearly dependent on earlier ones. Non-empty means the
    /// sketch is over-constrained (harmlessly, if the solve converged).
    pub redundant: Vec<usize>,
    /// Indices of constraints left unsatisfied (only when `Inconsistent`).
    pub failed: Vec<usize>,
    pub iterations: usize,
}

impl SolveResult {
    pub fn converged(&self) -> bool {
        self.status == SolveStatus::Converged
    }
}

pub(crate) fn solve(sk: &mut Sketch) -> SolveResult {
    let n = sk.params.len();
    let mut q = sk.params.clone();
    let mut r = sk.residuals(&q);
    let m = r.len();
    let mut iterations = 0;

    if m > 0 && n > 0 {
        let mut lambda = 1e-4;
        let mut cost = norm2(&r);
        for it in 0..MAX_ITER {
            iterations = it + 1;
            if max_abs(&r) < SATISFIED_TOL {
                break;
            }
            let jac = jacobian(sk, &q, m);
            // Normal equations: (JᵀJ + λ·diag(JᵀJ)) δ = −Jᵀr
            let mut jtj = mat_mul_t(&jac, n, m);
            let jtr = mat_t_vec(&jac, &r, n, m);
            // Try LM steps, growing λ until the step reduces the cost.
            let mut stepped = false;
            for _ in 0..24 {
                let mut a = jtj.clone();
                for i in 0..n {
                    // Marquardt scaling with an absolute floor so zero-gradient
                    // params (free directions) stay damped.
                    let d = jtj[i * n + i];
                    a[i * n + i] = d + lambda * d.max(1e-8);
                }
                let rhs: Vec<f64> = jtr.iter().map(|v| -v).collect();
                let Some(delta) = solve_dense(a, rhs, n) else {
                    lambda *= 10.0;
                    continue;
                };
                let q_new: Vec<f64> = q.iter().zip(&delta).map(|(a, d)| a + d).collect();
                let r_new = sk.residuals(&q_new);
                let cost_new = norm2(&r_new);
                if cost_new < cost || max_abs(&r_new) < SATISFIED_TOL {
                    q = q_new;
                    r = r_new;
                    cost = cost_new;
                    lambda = (lambda * 0.3).max(1e-12);
                    stepped = true;
                    break;
                }
                lambda *= 10.0;
            }
            if !stepped {
                break; // stalled — report best effort
            }
            // Re-derive jtj next iteration.
            jtj.clear();
        }
    }

    sk.params = q.clone();
    let r = sk.residuals(&q);

    // Diagnostics at the solution: rank + per-constraint redundancy.
    let jac = if m > 0 && n > 0 { jacobian(sk, &q, m) } else { Vec::new() };
    let (rank, redundant) = rank_and_redundant(sk, &jac, n);
    let dof = n.saturating_sub(rank);

    // Which constraints remain unsatisfied?
    let mut failed = Vec::new();
    let mut row = 0usize;
    for (ci, c) in sk.constraints.iter().enumerate() {
        let k = c.residual_count();
        if r[row..row + k].iter().any(|v| v.abs() > 1e-6) {
            failed.push(ci);
        }
        row += k;
    }
    let status = if failed.is_empty() { SolveStatus::Converged } else { SolveStatus::Inconsistent };
    SolveResult { status, dof, redundant, failed, iterations }
}

/// Numeric Jacobian (central differences), row-major m×n.
fn jacobian(sk: &Sketch, q: &[f64], m: usize) -> Vec<f64> {
    let n = q.len();
    let mut jac = vec![0.0; m * n];
    let mut qp = q.to_vec();
    for j in 0..n {
        let h = 1e-7 * q[j].abs().max(1.0);
        qp[j] = q[j] + h;
        let rp = sk.residuals(&qp);
        qp[j] = q[j] - h;
        let rm = sk.residuals(&qp);
        qp[j] = q[j];
        for i in 0..m {
            jac[i * n + j] = (rp[i] - rm[i]) / (2.0 * h);
        }
    }
    jac
}

/// Rank of the Jacobian plus the constraints whose rows are all linearly
/// dependent on rows of earlier constraints (modified Gram–Schmidt over rows,
/// walked in constraint order so "redundant" blames the later constraint).
fn rank_and_redundant(sk: &Sketch, jac: &[f64], n: usize) -> (usize, Vec<usize>) {
    let mut basis: Vec<Vec<f64>> = Vec::new();
    let mut redundant = Vec::new();
    let mut row = 0usize;
    for (ci, c) in sk.constraints.iter().enumerate() {
        let k = c.residual_count();
        let mut any_independent = false;
        for i in row..row + k {
            let mut v: Vec<f64> = jac[i * n..(i + 1) * n].to_vec();
            let orig = norm2(&v).sqrt();
            if orig <= RANK_TOL {
                continue; // degenerate row (zero gradient) — not counted
            }
            for b in &basis {
                let d = dot(&v, b);
                for (vj, bj) in v.iter_mut().zip(b) {
                    *vj -= d * bj;
                }
            }
            let rem = norm2(&v).sqrt();
            if rem > RANK_TOL * orig {
                let inv = 1.0 / rem;
                v.iter_mut().for_each(|x| *x *= inv);
                basis.push(v);
                any_independent = true;
            }
        }
        if !any_independent && k > 0 {
            redundant.push(ci);
        }
        row += k;
    }
    (basis.len(), redundant)
}

fn norm2(v: &[f64]) -> f64 {
    v.iter().map(|x| x * x).sum()
}

fn max_abs(v: &[f64]) -> f64 {
    v.iter().fold(0.0, |a, x| a.max(x.abs()))
}

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// JᵀJ for a row-major m×n J → n×n row-major.
fn mat_mul_t(jac: &[f64], n: usize, m: usize) -> Vec<f64> {
    let mut out = vec![0.0; n * n];
    for i in 0..n {
        for j in i..n {
            let mut s = 0.0;
            for k in 0..m {
                s += jac[k * n + i] * jac[k * n + j];
            }
            out[i * n + j] = s;
            out[j * n + i] = s;
        }
    }
    out
}

/// Jᵀr.
fn mat_t_vec(jac: &[f64], r: &[f64], n: usize, m: usize) -> Vec<f64> {
    let mut out = vec![0.0; n];
    for k in 0..m {
        for i in 0..n {
            out[i] += jac[k * n + i] * r[k];
        }
    }
    out
}

/// Dense Gaussian elimination with partial pivoting; `a` is n×n row-major.
/// Returns `None` when singular.
fn solve_dense(mut a: Vec<f64>, mut b: Vec<f64>, n: usize) -> Option<Vec<f64>> {
    for col in 0..n {
        // pivot
        let mut piv = col;
        let mut best = a[col * n + col].abs();
        for row in col + 1..n {
            let v = a[row * n + col].abs();
            if v > best {
                best = v;
                piv = row;
            }
        }
        if best < 1e-14 {
            return None;
        }
        if piv != col {
            for j in 0..n {
                a.swap(col * n + j, piv * n + j);
            }
            b.swap(col, piv);
        }
        let inv = 1.0 / a[col * n + col];
        for row in col + 1..n {
            let f = a[row * n + col] * inv;
            if f == 0.0 {
                continue;
            }
            for j in col..n {
                a[row * n + j] -= f * a[col * n + j];
            }
            b[row] -= f * b[col];
        }
    }
    // back-substitution
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let mut s = b[row];
        for j in row + 1..n {
            s -= a[row * n + j] * x[j];
        }
        x[row] = s / a[row * n + row];
    }
    Some(x)
}
