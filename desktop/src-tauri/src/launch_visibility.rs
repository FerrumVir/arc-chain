pub(crate) fn hide_on_launch(
    minimized: bool,
    has_tray: bool,
    has_identity: bool,
    has_config: bool,
) -> bool {
    minimized && has_tray && has_identity && has_config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_set_up_install_with_a_tray_can_start_hidden() {
        for minimized in [false, true] {
            for tray in [false, true] {
                for identity in [false, true] {
                    for config in [false, true] {
                        assert_eq!(
                            hide_on_launch(minimized, tray, identity, config),
                            minimized && tray && identity && config
                        );
                    }
                }
            }
        }
        assert!(!hide_on_launch(true, true, true, false));
        assert!(!hide_on_launch(true, true, false, true));
    }
}
