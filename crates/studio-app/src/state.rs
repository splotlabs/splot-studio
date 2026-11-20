// SPDX-License-Identifier: PolyForm-Noncommercial-1.0.0
// SPDX-FileCopyrightText: 2026 Bartosz Tomczyk <bartekplus@gmail.com>

//! Interaction state: what the user has chosen, as typed values.
//!
//! UI callbacks become [`Intent`]s; [`AppState::apply`] validates them against
//! the domain types and the window is then re-synchronised from [`AppState`],
//! so a rejected value snaps back instead of lingering in a widget.

use std::path::{Path, PathBuf};

use studio_domain::settings::{Crf, EncoderSettings, OutputKind, SpeedPreset};

/// `splot_encode::Config::DEFAULT_CRF` at the pinned codec revision.
const CODEC_DEFAULT_CRF: u8 = 20;

/// `splot_encode::Preset::default()` at the pinned codec revision.
const CODEC_DEFAULT_PRESET: u8 = 6;

/// The settings shown before the user changes anything.
///
/// Both values mirror the pinned codec revision: `Config::DEFAULT_CRF` and
/// `Preset::default()`. STUDIO-02 takes them from the engine instead.
// The `unreachable!` arms run in const evaluation: an out-of-range value is a
// compile error, never a runtime panic.
pub const INITIAL_SETTINGS: EncoderSettings = EncoderSettings {
    output: OutputKind::ALL[0],
    preset: match SpeedPreset::new(CODEC_DEFAULT_PRESET) {
        Ok(preset) => preset,
        Err(_) => unreachable!(),
    },
    crf: match Crf::new(CODEC_DEFAULT_CRF) {
        Ok(crf) => crf,
        Err(_) => unreachable!(),
    },
};

/// Something the user asked for through the interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Open the native file dialog.
    OpenRequested,
    /// The file dialog closed; `None` means it was cancelled.
    PickFinished(Option<PathBuf>),
    /// An output kind, by index into [`OutputKind::ALL`].
    OutputSelected(i32),
    /// A speed preset as typed or stepped in the widget.
    PresetEdited(i32),
    /// A rate factor as typed or stepped in the widget.
    CrfEdited(i32),
}

/// What [`AppState::apply`] asks the caller to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Only re-synchronise the window.
    None,
    /// Show the native file dialog.
    ShowOpenDialog,
}

/// The user's choices so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    /// The chosen source. Not opened, probed or validated in this build.
    source: Option<PathBuf>,
    /// The requested encoder settings.
    settings: EncoderSettings,
    /// A file dialog is showing; further Open requests are ignored.
    picking: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            source: None,
            settings: INITIAL_SETTINGS,
            picking: false,
        }
    }
}

impl AppState {
    /// Applies one intent. Out-of-range values leave the state unchanged.
    pub fn apply(&mut self, intent: Intent) -> Effect {
        match intent {
            Intent::OpenRequested if self.picking => return Effect::None,
            Intent::OpenRequested => {
                self.picking = true;
                return Effect::ShowOpenDialog;
            }
            Intent::PickFinished(path) => {
                self.picking = false;
                // Cancelling keeps the previous choice.
                if path.is_some() {
                    self.source = path;
                }
            }
            Intent::OutputSelected(index) => {
                if let Some(&output) = usize::try_from(index)
                    .ok()
                    .and_then(|i| OutputKind::ALL.get(i))
                {
                    self.settings.output = output;
                }
            }
            Intent::PresetEdited(value) => {
                if let Some(preset) = to_u8(value).and_then(|v| SpeedPreset::new(v).ok()) {
                    self.settings.preset = preset;
                }
            }
            Intent::CrfEdited(value) => {
                if let Some(crf) = to_u8(value).and_then(|v| Crf::new(v).ok()) {
                    self.settings.crf = crf;
                }
            }
        }
        Effect::None
    }

    /// The chosen source path.
    #[must_use]
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// The requested encoder settings.
    #[must_use]
    pub const fn settings(&self) -> EncoderSettings {
        self.settings
    }

    /// The index of the current output kind in [`OutputKind::ALL`].
    #[must_use]
    pub fn output_index(&self) -> i32 {
        OutputKind::ALL
            .iter()
            .position(|&kind| kind == self.settings.output)
            .and_then(|i| i32::try_from(i).ok())
            .unwrap_or(0)
    }

    /// The source's file name and full path for display. Lossy for paths that
    /// are not valid Unicode; the [`PathBuf`] itself stays exact.
    #[must_use]
    pub fn source_display(&self) -> Option<(String, String)> {
        let path = self.source.as_deref()?;
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Some((name, path.display().to_string()))
    }
}

fn to_u8(value: i32) -> Option<u8> {
    u8::try_from(value).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_is_not_reentrant_while_a_dialog_shows() {
        let mut state = AppState::default();
        assert_eq!(state.apply(Intent::OpenRequested), Effect::ShowOpenDialog);
        assert_eq!(state.apply(Intent::OpenRequested), Effect::None);
        state.apply(Intent::PickFinished(None));
        assert_eq!(state.apply(Intent::OpenRequested), Effect::ShowOpenDialog);
    }

    #[test]
    fn picking_sets_the_source_and_cancelling_keeps_it() {
        let mut state = AppState::default();
        state.apply(Intent::OpenRequested);
        state.apply(Intent::PickFinished(Some(PathBuf::from(
            "/media/clip one.y4m",
        ))));
        assert_eq!(
            state.source_display(),
            Some(("clip one.y4m".to_owned(), "/media/clip one.y4m".to_owned()))
        );
        state.apply(Intent::OpenRequested);
        state.apply(Intent::PickFinished(None));
        assert_eq!(state.source(), Some(Path::new("/media/clip one.y4m")));
    }

    #[test]
    fn settings_follow_the_domain_ranges() {
        let mut state = AppState::default();
        state.apply(Intent::PresetEdited(i32::from(SpeedPreset::MAX)));
        state.apply(Intent::CrfEdited(i32::from(Crf::MIN)));
        state.apply(Intent::OutputSelected(1));
        let expected = EncoderSettings {
            output: OutputKind::ALL[1],
            preset: SpeedPreset::new(SpeedPreset::MAX).unwrap_or_else(|e| unreachable!("{e}")),
            crf: Crf::new(Crf::MIN).unwrap_or_else(|e| unreachable!("{e}")),
        };
        assert_eq!(state.settings(), expected);
        assert_eq!(state.output_index(), 1);

        for bad in [
            Intent::PresetEdited(i32::from(SpeedPreset::MAX) + 1),
            Intent::PresetEdited(-1),
            Intent::CrfEdited(i32::from(Crf::MIN) - 1),
            Intent::CrfEdited(i32::from(Crf::MAX) + 1),
            Intent::CrfEdited(i32::MAX),
            Intent::OutputSelected(-1),
            Intent::OutputSelected(2),
        ] {
            state.apply(bad.clone());
            assert_eq!(state.settings(), expected, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn initial_settings_are_valid_and_first_output() {
        let state = AppState::default();
        assert_eq!(state.settings(), INITIAL_SETTINGS);
        assert_eq!(state.output_index(), 0);
        assert_eq!(state.source_display(), None);
    }
}
