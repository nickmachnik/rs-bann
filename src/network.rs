//! The network implementation

use ndarray::{arr1, s, Array1, ArrayView1};
use rand::prelude::ThreadRng;
use rand::{thread_rng, Rng};
use rs_bedvec::bedvec::BedVecCM;
use rs_bedvec::io::BedReader;
use rs_hmc::momentum::Momentum;
use rs_hmc::momentum::MultivariateStandardNormalMomentum;

type A = Array1<f32>;

#[inline(always)]
fn activation_fn(x: f32) -> f32 {
    f32::tanh(x)
}

#[inline(always)]
fn activation_fn_derivative(x: f32) -> f32 {
    1. - f32::tanh(x).powf(2.)
}

/// A group of markers
struct MarkerGroup {
    residual: A,
    w1: A,
    b1: f32,
    w2: f32,
    lambda_w1: f32,
    lambda_b1: f32,
    lambda_w2: f32,
    lambda_e: f32,
    bed_reader: BedReader,
    dim: usize,
    rng: ThreadRng,
    momentum_sampler: MultivariateStandardNormalMomentum,
    marker_data: Option<BedVecCM>,
}

impl MarkerGroup {
    fn new(
        residual: A,
        w1: A,
        b1: f32,
        w2: f32,
        bed_reader: BedReader,
        dim: usize,
    ) -> Self {
        Self {
            residual,
            w1,
            b1,
            w2,
            lambda_w1: 1.,
            lambda_b1: 1.,
            lambda_w2: 1.,
            lambda_e: 1.,
            bed_reader,
            dim,
            rng: thread_rng(),
            momentum_sampler: MultivariateStandardNormalMomentum::new(dim + 2),
            marker_data: None,
        }
    }

    fn load_marker_data(&mut self) {
        self.marker_data = Some(self.bed_reader.read_into_bedvec());
    }

    fn forget_marker_data(&mut self) {
        self.marker_data = None;
    }

    fn forward_feed(&self, b1: f32, w1: &ArrayView1<f32>, w2: f32) -> A {
        // this does not include b2, because it is not group specific
        (self
            .marker_data
            .as_ref()
            .unwrap()
            .right_multiply_par(w1.as_slice().unwrap())
            + b1)
            .mapv(activation_fn)
            * w2
    }

    fn rss(&self, b1: f32, w1: &ArrayView1<f32>, w2: f32) -> f32 {
        let r = &self.residual - self.forward_feed(b1, w1, w2);
        r.dot(&r)
    }

    // logarithm of the parameter density (-U)
    // this has to accept a parameter vector
    fn log_density(&self, param_vec: &A) -> f32 {
        let b1_index = 0;
        let w1_index_first = 1;
        let w1_index_last = self.dim;
        let w2_index = w1_index_last + 1;
        let b1 = param_vec[b1_index];
        let w1 = param_vec.slice(s![w1_index_first..=w1_index_last]);
        let w2 = param_vec[w2_index];
        let b1_part = -self.lambda_b1 / 2. * b1 * b1;
        let w1_part = -self.lambda_w1 / 2. * w1.dot(&w1);
        let w2_part = -self.lambda_w2 / 2. * w2 * w2;
        let rss_part = self.lambda_e / 2. * self.rss(b1, &w1, w2);
        b1_part + w1_part + w2_part + rss_part
    }

    fn log_density_gradient(&self, param_vec: &A) -> A {
        let b1_index = 0;
        let w1_index_first = 1;
        let w1_index_last = self.dim;
        let w2_index = w1_index_last + 1;
        let b1 = param_vec[0];
        let w1 = param_vec.slice(s![w1_index_first..=w1_index_last]);
        let w2 = param_vec[w2_index];
        let x_times_w1 = self
            .marker_data
            .as_ref()
            .unwrap()
            .right_multiply_par(w1.as_slice().unwrap());
        let z = &x_times_w1 + b1;
        let a = (x_times_w1 + b1).mapv(activation_fn);
        let y_hat = &a * &self.w1;
        let h_prime_of_z = z.mapv(activation_fn_derivative);
        let drss_dyhat = -self.lambda_e * (y_hat - &self.residual);
        let mut gradient: A = Array1::zeros(2 + w1.len());

        gradient[b1_index] = -self.lambda_b1 * b1 + w2 * drss_dyhat.dot(&h_prime_of_z);
        gradient
            .slice_mut(s![w1_index_first..=w1_index_last])
            .assign(
                &(-self.lambda_w1 * &w1
                    + (&drss_dyhat
                        * w2
                        * self
                            .marker_data
                            .as_ref()
                            .unwrap()
                            .left_multiply_simd_v1_par(h_prime_of_z.as_slice().unwrap()))),
            );
        gradient[w2_index] = -self.lambda_w2 * w2 + drss_dyhat.dot(&a);
        gradient
    }

    fn param_vec(&self) -> A {
        let mut p = Vec::with_capacity(self.w1.len() + 1);
        p.push(self.b1);
        p.extend(&self.w1);
        arr1(&p)
    }

    // Take single sample using HMC
    // TODO: could to max tries and reduce step size if unsuccessful
    fn sample_params(&mut self, step_size: f32, integration_length: usize) -> A {
        let start_position = self.param_vec();
        loop {
            let mut position = start_position.clone();
            let start_momentum: A = self.momentum_sampler.sample();
            let mut momentum = start_momentum.clone();
            for _ in 0..integration_length {
                self.leapfrog(&mut position, &mut momentum, step_size);
            }
            let acc_prob =
                self.acceptance_probability(&position, &momentum, &start_position, &start_momentum);
            if self.accept(acc_prob) {
                return position;
            }
        }
    }

    fn accept(&mut self, acceptance_probability: f32) -> bool {
        self.rng.gen_range(0.0..1.0) < acceptance_probability
    }

    fn acceptance_probability(
        &self,
        new_position: &A,
        new_momentum: &A,
        initial_position: &A,
        initial_momentum: &A,
    ) -> f32 {
        let log_acc_probability = self.neg_hamiltonian(new_position, new_momentum)
            - self.neg_hamiltonian(initial_position, initial_momentum);
        if log_acc_probability >= 0. {
            return 1.;
        }
        log_acc_probability.exp()
    }

    // this is -H = (-U) + (-K)
    fn neg_hamiltonian(&self, position: &A, momentum: &A) -> f32 {
        self.log_density(position) + self.momentum_sampler.log_density(momentum)
    }

    fn leapfrog(&self, position: &mut A, momentum: &mut A, step_size: f32) {
        momentum.scaled_add(step_size / 2., &self.log_density_gradient(position));
        position.scaled_add(step_size, momentum);
        momentum.scaled_add(step_size / 2., &self.log_density_gradient(position));
    }
}

// Will have multiple groups
// Each group should be trained independently
// e.g. each group will have it's own sampler
pub struct Net {}

impl Net {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_marker_group_param_sampling() {
        let mg = MarkerGroup::new(residual: arr1(&[0., 1., 2.,]), w1: [1., 1.], b1: 1., w2: 1., bed_reader: BedReader, dim: 2)
    }
}