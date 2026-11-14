//! Which Vulkan driver a window will use — decided **before** GPUI asks for one.
//!
//! GPUI initialises its renderer inside `Application::new`, and with no usable
//! driver that is a `panic!("Unable to init GPU context: NoSupportedDeviceFound")`
//! from `gpui-0.2.2/src/platform/linux/wayland/client.rs`. The panic says nothing
//! about the machine, so it reads like a bug in this app; on a box whose `/dev`
//! has no render node it is a missing driver instead, and the fix is a command.
//! This module exists to say that in words, once, before the panic can happen.
//!
//! Two rules keep the diagnosis honest rather than superstitious:
//!
//! * **only provable absence is reported.** A configured ICD wins, a real render
//!   node wins, and a *software* ICD (Mesa lavapipe) wins even without a device,
//!   because that is the one driver that genuinely renders on a CPU. Anything
//!   else — a custom ICD this build cannot reason about — is left to GPUI: this
//!   module has no business refusing a machine that might work.
//! * **a directory is not a device.** `/dev/dri` can exist and be empty (a
//!   sandbox that bind-mounts an empty dir over it does exactly that), and a
//!   zero-length `/dev/nvidia0` is a placeholder, not a GPU. Both were measured
//!   while writing this module: `test -e /dev/dri` is `yes` with the directory
//!   completely empty. So the device check looks for a **render node** — an
//!   `renderD*` or `card*` entry inside `/dev/dri`, or a non-empty
//!   `/dev/nvidia0` — because the weaker check would pass and then panic, which
//!   is the one outcome this module exists to prevent.
//!
//! The decision is a pure function over three booleans, and the two booleans come
//! from small filesystem helpers, so the table below is pinned by unit tests
//! instead of by trying to boot a window on a machine with no GPU.

use std::os::unix::fs::FileTypeExt;

/// Environment variables the Vulkan loader honours for an explicit ICD.
///
/// `VK_ICD_FILENAMES` is the long-standing name; `VK_DRIVER_FILES` is the
/// replacement and is set *instead of* it on newer loaders. Either one means the
/// caller has already chosen — this build does not second-guess that.
pub const ICD_ENV_VARS: [&str; 2] = ["VK_ICD_FILENAMES", "VK_DRIVER_FILES"];

/// Where Mesa puts DRM nodes. The directory is checked by *contents*, never by
/// existence (see the module docs).
pub const DRI_DIR: &str = "/dev/dri";

/// NVIDIA's own device node.
pub const NVIDIA_DEVICE: &str = "/dev/nvidia0";

/// Mesa's software rasteriser (lavapipe) as a distro installs it. Its presence is
/// enough on its own: lavapipe is a CPU driver and needs no DRM node, which is
/// exactly why it is the fallback for a sandbox or a headless box.
pub const SOFTWARE_ICDS: [&str; 2] = [
    "/usr/share/vulkan/icd.d/lvp_icd.x86_64.json",
    "/usr/share/vulkan/icd.d/lvp_icd.json",
];

/// The software ICD `scripts/software-vulkan.sh` stages inside the checkout.
/// Named separately from the system paths above so the report line can say which
/// one is doing the rendering — a staged driver is a different fact from an
/// installed one.
pub const STAGED_ICD: &str = ".tmp-ui-gpu/lvp_local.json";

/// What the machine has, as far as the window's renderer is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Driver {
    /// `VK_ICD_FILENAMES` / `VK_DRIVER_FILES` was set by whoever started us
    /// (`scripts/env.sh` does this). Trusted as-is: the caller made a decision.
    Configured,
    /// A DRM render node (or NVIDIA's own device) is present.
    Device,
    /// No device, but a software ICD is installed: rendering works on the CPU.
    Software,
    /// None of the above. GPUI would panic; say why instead.
    Absent,
}

impl Driver {
    /// The phrase a report line uses. No claim beyond what was checked.
    pub fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured by the environment",
            Self::Device => "a DRM render node is present",
            Self::Software => "a software ICD is installed (lavapipe)",
            Self::Absent => "no Vulkan driver is reachable",
        }
    }

    /// Whether a window has any chance of initialising a renderer.
    pub fn can_render(self) -> bool {
        !matches!(self, Self::Absent)
    }
}

/// The decision table. Pure, so every row is a test.
pub fn decide(configured: bool, device: bool, software: bool) -> Driver {
    if configured {
        return Driver::Configured;
    }
    match (device, software) {
        (true, _) => Driver::Device,
        (false, true) => Driver::Software,
        (false, false) => Driver::Absent,
    }
}

/// Whether a `/dev/dri` entry names a usable device node.
///
/// Mesa's KMS nodes are `card*` and its render nodes `renderD*`; anything else in
/// that directory (a `by-path` symlink directory, a stray file) is not a device
/// this build can render with. Pure so the rule is stated once and tested.
pub fn looks_like_render_node(name: &str) -> bool {
    let digits = name
        .strip_prefix("renderD")
        .or_else(|| name.strip_prefix("card"));
    // A prefix with no number after it (`renderD`, `card`) is not a node, and
    // neither is a name that merely starts like one (`cardbackup`).
    matches!(digits, Some(rest) if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// Probe the filesystem for a real DRM device. A directory that exists but holds
/// no node counts as absent, and a zero-length `/dev/nvidia0` placeholder does
/// too — both are what a sandboxed `/dev` looks like.
pub fn device_present() -> bool {
    let dri = std::fs::read_dir(DRI_DIR)
        .map(|entries| {
            entries
                .flatten()
                .any(|entry| looks_like_render_node(&entry.file_name().to_string_lossy()))
        })
        .unwrap_or(false);
    let nvidia = std::fs::metadata(NVIDIA_DEVICE)
        .map(|meta| meta.len() > 0 || meta.file_type().is_char_device())
        .unwrap_or(false);
    dri || nvidia
}

/// Probe the filesystem for a *software* Vulkan ICD the loader finds **by
/// itself**: the distro's, in `/usr/share/vulkan/icd.d`.
///
/// The ICD `scripts/software-vulkan.sh` stages is deliberately *not* counted
/// here. It lives in this checkout, which the loader never scans, so its presence
/// proves nothing on its own — it becomes usable exactly when `scripts/env.sh`
/// exports `VK_ICD_FILENAMES` to it, and that case is [`Driver::Configured`].
/// Counting it here would tell a run "a software ICD is installed" and then let
/// GPUI panic, which is the failure this module exists to prevent.
pub fn software_present() -> bool {
    SOFTWARE_ICDS
        .iter()
        .any(|path| std::path::Path::new(path).exists())
}

/// Whether the environment already names an ICD.
pub fn configured_by_env() -> bool {
    ICD_ENV_VARS
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
}

/// [`decide`] against this process's real environment and filesystem.
pub fn diagnose_here() -> Driver {
    decide(configured_by_env(), device_present(), software_present())
}

/// The sentence a run prints when the renderer cannot possibly start.
///
/// It names what was checked (so an operator can disagree with the conclusion),
/// what is missing, and the one command that fixes it without root. It never
/// claims the app is broken, because it is not.
pub fn absent_message() -> String {
    format!(
        "no Vulkan driver: neither {} nor {} names one, {DRI_DIR} holds no render node, {NVIDIA_DEVICE} is absent, \
and no system software ICD ({}) is installed. The window cannot open, and no render happens without one. \
Fix without root: `scripts/software-vulkan.sh` stages Mesa lavapipe inside this checkout and writes {STAGED_ICD}; \
then `source scripts/env.sh` (which exports {} to it) and re-run this command.",
        ICD_ENV_VARS[0],
        ICD_ENV_VARS[1],
        SOFTWARE_ICDS.join(" / "),
        ICD_ENV_VARS[0]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The decision table, including the case this module exists for: an
    /// operator sets nothing, `/dev` has no render node, and the only ICD on the
    /// box is a hardware one whose device is absent — which is what a sandboxed
    /// session sees.
    #[test]
    fn the_driver_table_says_absent_only_when_it_can_prove_it() {
        // A configured ICD wins regardless of devices, because deciding
        // otherwise would second-guess `scripts/env.sh` and any operator who
        // exported a driver on purpose.
        assert_eq!(decide(true, false, false), Driver::Configured);
        assert_eq!(decide(true, true, true), Driver::Configured);

        // Hardware beats software when both are reachable.
        assert_eq!(decide(false, true, false), Driver::Device);
        assert_eq!(decide(false, true, true), Driver::Device);

        // No device, software ICD installed: usable, and it says which.
        assert_eq!(decide(false, false, true), Driver::Software);

        // Nothing at all: the one refusal.
        assert_eq!(decide(false, false, false), Driver::Absent);
    }

    /// A `/dev/dri` entry is a render node only when it really names one. The
    /// empty-directory and placeholder-file cases are what a sandboxed `/dev`
    /// contains, and `test -e /dev/dri` is `yes` for both — which is why the
    /// device probe lists the directory instead of testing it.
    #[test]
    fn only_a_real_render_node_counts_as_a_device() {
        assert!(looks_like_render_node("renderD128"));
        assert!(looks_like_render_node("renderD129"));
        assert!(looks_like_render_node("card0"));
        assert!(looks_like_render_node("card1"));

        assert!(
            !looks_like_render_node("by-path"),
            "a symlink dir is not a node"
        );
        assert!(
            !looks_like_render_node("renderD"),
            "a bare prefix is not a node"
        );
        assert!(
            !looks_like_render_node("card"),
            "a bare prefix is not a node"
        );
        assert!(!looks_like_render_node("renderD12x"), "not a number");
        assert!(!looks_like_render_node(""), "nothing is not a node");
    }

    /// `can_render` is the whole point of the enum: only `Absent` is a refusal,
    /// and the labels are distinct so a report line names exactly one thing.
    #[test]
    fn every_reachable_driver_can_render_and_only_absence_refuses() {
        assert!(Driver::Configured.can_render());
        assert!(Driver::Device.can_render());
        assert!(Driver::Software.can_render());
        assert!(!Driver::Absent.can_render());

        let labels = [
            Driver::Configured.label(),
            Driver::Device.label(),
            Driver::Software.label(),
            Driver::Absent.label(),
        ];
        for (index, label) in labels.iter().enumerate() {
            assert!(
                !labels[index + 1..].contains(label),
                "two states share the label `{label}`"
            );
        }
    }

    /// The message has to be actionable on its own: it names the script, the
    /// environment step, and never claims the app is at fault.
    #[test]
    fn the_absent_message_names_the_fix_not_a_bug() {
        let message = absent_message();
        assert!(message.contains("scripts/software-vulkan.sh"), "{message}");
        assert!(message.contains("source scripts/env.sh"), "{message}");
        assert!(message.contains("without root"), "{message}");
        assert!(message.contains("VK_ICD_FILENAMES"), "{message}");
        assert!(message.contains("render node"), "{message}");
        for misleading in ["panic", "bug", "crash", "unsupported"] {
            assert!(!message.contains(misleading), "{misleading}: {message}");
        }
    }

    /// The live probe must agree with the table on *this* machine: whichever
    /// driver it reports, `can_render` and `diagnose_here` must not disagree, and
    /// an `Absent` verdict must come with nothing configured.
    #[test]
    fn the_live_probe_is_internally_consistent() {
        let driver = diagnose_here();
        assert_eq!(driver.can_render(), driver != Driver::Absent);
        if configured_by_env() {
            assert_eq!(driver, Driver::Configured);
        }
    }
}
