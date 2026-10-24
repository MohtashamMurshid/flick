//! Software presentation sizing until native GPU presentation is available.

/// Match physical display pixels. There is no application resolution cap.
/// The native renderer checks representability and allocation separately.
pub fn target_size(width: f32, height: f32, scale_factor: f32) -> (u32, u32) {
    let valid = |v: f32| {
        if v.is_finite() && v > 0. {
            f64::from(v)
        } else {
            1.
        }
    };
    let width = valid(width) * valid(scale_factor);
    let height = valid(height) * valid(scale_factor);
    (width.round().max(1.) as u32, height.round().max(1.) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retina_and_full_hd_are_not_downscaled_to_720p() {
        assert_eq!(target_size(1920., 1080., 1.), (1920, 1080));
        assert_eq!(target_size(1920., 1080., 2.), (3840, 2160));
    }

    #[test]
    fn larger_displays_are_not_clamped_to_4k_or_8192_pixels() {
        assert_eq!(target_size(7680., 4320., 1.), (7680, 4320));
        assert_eq!(target_size(7680., 4320., 2.), (15360, 8640));
        assert_eq!(target_size(10240., 1440., 1.), (10240, 1440));
        assert_eq!(target_size(f32::NAN, 0., f32::INFINITY), (1, 1));
    }
}
