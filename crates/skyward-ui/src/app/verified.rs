//! The page that stops the bootloader checking what it boots: what it
//! reads, and the switches that change it.

use super::*;

impl App {
    /// What the bootloader still checks, where the runtime could read it.
    pub fn verified(&self) -> Option<Verified> {
        match &self.state.verified {
            Some(Ok(read)) => Some(*read),
            _ => None,
        }
    }

    /// The bootloader's checks have been read, or could not be.
    pub fn set_verified(&mut self, read: Result<Verified, String>) {
        self.state.verified = Some(read);
        self.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::testing::*;

    #[test]
    fn the_page_reads_the_checks_as_it_opens_and_switches_them_off() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Advanced, W, H, PPD);
        assert_eq!(tap(&mut app, &f, &i, Action::Open(Screen::Verified)), vec![Effect::ReadVerified]);
        app.set_verified(Ok(Verified { verification: true, verity: true }));
        assert!(app.verified().unwrap().verification);
        let effects = tap(&mut app, &f, &i, Action::Toggle(Switch::Verification));
        assert_eq!(effects, vec![Effect::SetVerification(false)], "the switch asks for the check to stop");
        app.set_verified(Ok(Verified { verification: false, verity: true }));
        assert_eq!(app.verified(), Some(Verified { verification: false, verity: true }));
        assert_eq!(
            tap(&mut app, &f, &i, Action::Toggle(Switch::Verity)),
            vec![Effect::SetVerity(false)],
            "each check is its own"
        );
    }

    #[test]
    fn a_phone_whose_checks_could_not_be_read_says_so_and_can_still_be_asked_to_stop_them() {
        let (f, i) = (fonts(), info());
        let mut app = App::new(Screen::Verified, W, H, PPD);
        app.set_verified(Err("no vbmeta partition".into()));
        assert_eq!(app.verified(), None);
        assert_eq!(tap(&mut app, &f, &i, Action::Toggle(Switch::Verity)), vec![Effect::SetVerity(false)]);
    }
}
