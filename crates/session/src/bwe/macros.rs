//! Estimator stat taps: trace-level, keyed for offline analysis.

macro_rules! bwe_stat {
    ($name:literal, $($arg:expr),+ $(,)?) => {
        tracing::trace!(target: "bwe_stat", stat = $name, values = ?($(&$arg),+));
    }
}

macro_rules! log_inherent_loss { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("inherent_loss", $($a),+) } }
macro_rules! log_loss_bw_limit_in_window { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("loss_bw_limit_in_window", $($a),+) } }
macro_rules! log_probe_bitrate_estimate { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("probe_bitrate_estimate", $($a),+) } }
macro_rules! log_rate_control_applied_change { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("rate_control_applied_change", $($a),+) } }
macro_rules! log_rate_control_observed_bitrate { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("rate_control_observed_bitrate", $($a),+) } }
macro_rules! log_rate_control_state { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("rate_control_state", $($a),+) } }
macro_rules! log_delay_variation { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("delay_variation", $($a),+) } }
macro_rules! log_trendline_estimate { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("trendline_estimate", $($a),+) } }
macro_rules! log_trendline_modified_trend { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("trendline_modified_trend", $($a),+) } }
macro_rules! log_bitrate_estimate { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("bitrate_estimate", $($a),+) } }
macro_rules! log_loss_based_bitrate_estimate { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("loss_based_bitrate_estimate", $($a),+) } }
macro_rules! log_loss { ($($a:expr),+ $(,)?) => { crate::bwe::macros::bwe_stat!("loss", $($a),+) } }

pub(crate) use {
    bwe_stat, log_bitrate_estimate, log_delay_variation, log_inherent_loss, log_loss,
    log_loss_based_bitrate_estimate, log_loss_bw_limit_in_window, log_probe_bitrate_estimate,
    log_rate_control_applied_change, log_rate_control_observed_bitrate, log_rate_control_state,
    log_trendline_estimate, log_trendline_modified_trend,
};
