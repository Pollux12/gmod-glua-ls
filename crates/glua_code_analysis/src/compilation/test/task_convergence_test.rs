#[cfg(test)]
mod test {
    use crate::{Emmyrc, VirtualWorkspace};

    /// A weapon base that keeps a copy of its fire sound and restores it from
    /// another file, as CityRP's `cw_base` does: each statement's read waits
    /// while the other's write is untyped, so tasks that both run from one
    /// snapshot see each other's last state and swap it every superstep.
    const SHARED: &str = r#"
        function SWEP:Initialize()
            if self.FireSound then
                self.FireSound = Sound(CustomizableWeaponry:findFireSound(self.FireSound))
                self.FireSound_Orig = self.FireSound
            end
        end
    "#;
    const GENERAL: &str = r#"
        function SWEP:restoreSound()
            self.FireSound = self.FireSound_Orig
        end
    "#;

    /// What the copy cycle settles to, how many files and stages fell back
    /// to the serial loop, and how many derivations of a cycle ran on the
    /// index.
    fn settle() -> (Vec<String>, usize, usize) {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        ws.def_files(vec![
            ("entities/weapons/cw_base/shared.lua", SHARED),
            ("entities/weapons/cw_base/sh_general.lua", GENERAL),
        ]);
        let stats = ws.analysis.compilation.last_stats();
        let (fallbacks, cycle_members) = (stats.serial_fallbacks(), stats.cycle_members());
        (ws.settled_digest(), fallbacks, cycle_members)
    }

    /// Two files whose statements copy a field into each other settle in
    /// worker supersteps as the serial loop settles them: the cycle runs on
    /// the index, and no file falls back to the serial loop.
    #[test]
    fn fields_copied_into_each_other_across_files_settle_as_the_serial_loop_settles_them() {
        let (settled, fallbacks, cycle_members) = settle();
        let serial = {
            let _serial = crate::compilation::analyzer::serial_engine();
            settle().0
        };

        assert_eq!((fallbacks, cycle_members > 0, settled), (0, true, serial));
    }
}
