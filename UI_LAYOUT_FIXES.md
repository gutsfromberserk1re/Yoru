# Yoru editor layout / graphics fixes

This source update includes:

- Bounds the docked timeline panel so it cannot take most of a short window, and removes repeated full-available-size minimum constraints that could destabilize dock layout.
- Replaces the solid navy panel fill with a graphite / amber / steel / rose gradient inspired by the window backdrop.
- Starts a clean timeline with V1 and A1, then reveals up to V8 / A8 as clips are dragged into new lanes. Video and audio rows keep their stable lane slots for locking, visibility, and saved projects.
- Moves clip transitions into a dedicated **Transitions** tab beside Project and Effects, with searchable transition types and an adjustable duration. The old Transition In selector is removed from the top of Effect Controls.
- Adds a **T** shortcut to create a title at the playhead and focus its text editor. Essential Graphics prioritizes title editing and keeps shape creation collapsed; text, typography, appearance, position, scale, rotation, and opacity are editable there.
- Expands X/Y position beyond the 1920×1080 canvas and rotation to ±900°, so clips and titles can animate in from well off-screen.
- Fits the value graph to animated Position/Rotation values and constrains its height to avoid forcing dock changes.
- Fuses a final Flash/Fade effect into the compositor, avoiding an extra full-frame allocation/pass while preserving effect-stack order.

The source has been reviewed and the archive is integrity-checked. This environment does not have Cargo/Rust installed, so the Windows GitHub Actions build is still the required compile check.
