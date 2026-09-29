//! Host input-source handling for review sessions.

#[cfg(any(target_os = "macos", test))]
const ABC_INPUT_SOURCE: &str = "com.apple.keylayout.ABC";

/// Start an interactive review with macOS's plain Latin input source.
///
/// Input-source failures must not prevent a review from opening. The reviewer can still
/// switch manually, which is better than turning a convenience into a startup failure.
pub(crate) fn switch_to_abc() {
    #[cfg(target_os = "macos")]
    let _ = switch_to_abc_with(im_switch::get_input_method, im_switch::set_input_method);
}

#[cfg(any(target_os = "macos", test))]
fn switch_to_abc_with<E>(
    current: impl FnOnce() -> Result<String, E>,
    select: impl FnOnce(&str) -> Result<(), E>,
) -> Result<(), E> {
    if current()? == ABC_INPUT_SOURCE { Ok(()) } else { select(ABC_INPUT_SOURCE) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn abc_is_left_unchanged() {
        let selected = Cell::new(false);

        let result = switch_to_abc_with(
            || Ok::<_, ()>(ABC_INPUT_SOURCE.to_owned()),
            |_| {
                selected.set(true);
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        assert!(!selected.get());
    }

    #[test]
    fn a_non_abc_source_is_changed_to_abc() {
        let selected = Cell::new(false);

        let result = switch_to_abc_with(
            || Ok::<_, ()>("com.apple.inputmethod.SCIM.ITABC".to_owned()),
            |source| {
                assert_eq!(source, ABC_INPUT_SOURCE);
                selected.set(true);
                Ok(())
            },
        );

        assert_eq!(result, Ok(()));
        assert!(selected.get());
    }

    #[test]
    fn a_failed_current_source_lookup_does_not_try_to_switch() {
        let selected = Cell::new(false);

        let result = switch_to_abc_with(
            || Err::<String, _>("unavailable"),
            |_| {
                selected.set(true);
                Ok(())
            },
        );

        assert_eq!(result, Err("unavailable"));
        assert!(!selected.get());
    }
}
