//! Native Tag Manager viewport.
//!
//! This is a separate OS window, not an embedded sidebar panel. It deliberately
//! borrows the app's existing review state: there is one `TagReviewStore`, one
//! `ReviewUiState`, and one deferred `ReviewOutcome` path shared with the main
//! window and the media viewer.

use crate::gallery::{ClaimScope, ConfidenceRange, Query, Sidecar, TagNode};
use crate::query_composer;
use crate::source_search::ReviewOutcome;
use crate::tag_review::{draw_tag_review, ReviewUiState, TagReviewStore};
use egui::{Color32, RichText, TextEdit, Ui, ViewportBuilder, ViewportClass, ViewportId};
use std::path::Path;

/// Keep the picker usable even in folders with tens of thousands of images.
const MAX_TARGET_ROWS: usize = 200;

/// Stable id so the compositor keeps the same native window across frames.
pub fn viewport_id() -> ViewportId {
    ViewportId::from_hash_of("tag_manager")
}

#[derive(Clone, Debug)]
struct TargetEntry {
    path: String,
    key: String,
}

impl TargetEntry {
    fn new(path: String) -> Self {
        let key = path.to_lowercase();
        Self { path, key }
    }
}

/// Target list is built once when the window opens, not on every frame.
#[derive(Clone, Debug, Default)]
pub struct TargetPickerState {
    entries: Vec<TargetEntry>,
    search: String,
    applied_search: String,
    visible: Vec<usize>,
}

impl TargetPickerState {
    pub fn set_paths(&mut self, paths: Vec<String>) {
        self.entries = paths.into_iter().map(TargetEntry::new).collect();
        self.search.clear();
        self.applied_search.clear();
        self.visible = (0..self.entries.len()).take(MAX_TARGET_ROWS).collect();
    }

    /// Recompute the visible rows only when the filter text changes.
    fn visible(&mut self) -> &[usize] {
        if self.search != self.applied_search {
            let needle = self.search.trim().to_lowercase();
            self.visible.clear();
            for (index, entry) in self.entries.iter().enumerate() {
                if needle.is_empty() || entry.key.contains(&needle) {
                    self.visible.push(index);
                    if self.visible.len() >= MAX_TARGET_ROWS {
                        break;
                    }
                }
            }
            self.applied_search = self.search.clone();
        }
        &self.visible
    }
}

/// Prompt text is owned by Tag Manager. It compiles into the shared `Query`
/// AST but never writes tags or sidecars.
#[derive(Clone, Debug)]
pub struct AiSearchState {
    pub prompt: String,
    /// Feedback from the local query planner. Empty until Rebuild/Add is used.
    pub status: String,
    /// When true, the compiled query may also match accepted human tags. AI
    /// queries include them by default because the model uses all known image
    /// data; the checkbox exists for explicit machine-only searches.
    pub include_human: bool,
}

impl Default for AiSearchState {
    fn default() -> Self {
        Self { prompt: String::new(), status: String::new(), include_human: true }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TagManagerState {
    pub open: bool,
    /// Item id currently being reviewed. The user can change it from the window.
    pub item: Option<String>,
    pub targets: TargetPickerState,
    pub ai_search: AiSearchState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AiApplyMode {
    /// Replace the gallery filter with this query.
    Rebuild,
    /// AND this query onto the gallery's current filter.
    Add,
}

/// A previewed AI search the user explicitly chose to apply to the gallery.
#[derive(Clone, Debug)]
pub struct AiSearchApply {
    pub query: Query,
    pub confidence: ConfidenceRange,
    pub prompt: String,
    pub scope: ClaimScope,
    pub mode: AiApplyMode,
}

#[derive(Clone, Debug, Default)]
pub struct TagManagerOutcome {
    pub review: ReviewOutcome,
    /// A different image was chosen in the manager. The app loads its sidecar
    /// after the viewport closes its snapshot.
    pub target_changed: Option<String>,
    /// Duplicate search now lives with the metadata tools instead of the main
    /// toolbar.
    pub find_duplicates: bool,
    pub ai_apply: Option<AiSearchApply>,
}

fn short_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn draw_contents(
    ui: &mut Ui,
    state: &mut TagManagerState,
    review: &mut TagReviewStore,
    review_ui: &mut ReviewUiState,
    staged_sidecar: &mut Option<Sidecar>,
    tag_tree: &TagNode,
    gallery_query: &Query,
    outcome: &mut TagManagerOutcome,
) {
    let TagManagerState {
        open,
        item,
        targets,
        ai_search,
    } = state;
    let item_id = item.clone().unwrap_or_default();

    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("TAG MANAGER").strong());
        ui.separator();
        ui.label("Target");
        let selected = if item_id.is_empty() {
            "No image selected".to_string()
        } else {
            short_name(&item_id).to_string()
        };
        egui::ComboBox::from_id_source("tag_manager_target")
            .selected_text(selected)
            .width(260.0)
            .show_ui(ui, |ui| {
                let search = ui.add_sized(
                    [240.0, 20.0],
                    TextEdit::singleline(&mut targets.search)
                        .id_source("tag_manager_target_search")
                        .hint_text("Filter images…"),
                );
                // The combo keeps focus on its own button by default. Without
                // this, typing goes nowhere and the filter appears broken.
                search.request_focus();
                let visible = targets.visible().to_vec();
                ui.label(
                    RichText::new(format!("{} match(es)", visible.len()))
                        .size(10.0)
                        .color(Color32::GRAY),
                );
                ui.separator();
                if targets.entries.is_empty() {
                    ui.label(
                        RichText::new("No images in this folder")
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                    return;
                }
                for index in visible {
                    let entry = &targets.entries[index];
                    let is_current = item.as_deref() == Some(entry.path.as_str());
                    if ui.selectable_label(is_current, short_name(&entry.path)).clicked()
                        && !is_current
                    {
                        outcome.target_changed = Some(entry.path.clone());
                    }
                }
                if targets.visible.len() >= MAX_TARGET_ROWS && targets.entries.len() > MAX_TARGET_ROWS
                {
                    ui.label(
                        RichText::new(format!(
                            "Showing first {MAX_TARGET_ROWS}; refine the filter"
                        ))
                        .size(10.0)
                        .color(Color32::GRAY),
                    );
                }
            });
        if !item_id.is_empty() {
            ui.label(RichText::new(short_name(&item_id)).size(10.0).color(Color32::GRAY))
                .on_hover_text(&item_id);
        }
        if ui.button("Find duplicates…").on_hover_text(
            "Search the library for duplicate files",
        ).clicked() {
            outcome.find_duplicates = true;
        }
        if ui.small_button("Close").clicked() {
            *open = false;
        }
    });
    ui.separator();

    ui.collapsing("AI QUERY", |ui| {
        ui.label(
            RichText::new("Describe what to find. A local model plans against known tags before Rebuild/Add; it can map new names like sprigatito onto tags the library already understands. You can then tag the filtered images manually.")
                .size(10.0)
                .color(Color32::GRAY),
        );
        ui.add_sized(
            [ui.available_width(), 54.0],
            TextEdit::multiline(&mut ai_search.prompt)
                .hint_text("orange cat on a crate, possessive mood, by kitty"),
        );
        ui.horizontal(|ui| {
            ui.checkbox(&mut ai_search.include_human, "Include human tags")
                .on_hover_text("Let the query match accepted human tags as well as AI/source claims");
            ui.label(
                RichText::new(if ai_search.include_human {
                    "all claims"
                } else {
                    "machine claims only"
                })
                .size(10.0)
                .color(Color32::GRAY),
            );
        });

        let parsed = query_composer::compile_tree(&ai_search.prompt, tag_tree);
        let scope = if ai_search.include_human {
            ClaimScope::All
        } else {
            ClaimScope::Machine
        };
        let confidence = parsed.confidence_hint.unwrap_or_else(ConfidenceRange::all);
        ui.label(
            RichText::new(format!(
                "Compiled: {} · {} term(s)",
                parsed.query,
                parsed.term_count()
            ))
            .size(10.0)
            .color(Color32::GRAY),
        );
        if !ai_search.status.is_empty() {
            ui.label(
                RichText::new(&ai_search.status)
                    .size(10.0)
                    .color(Color32::from_rgb(255, 190, 110)),
            );
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    parsed.query != Query::AlwaysTrue,
                    egui::Button::new("Rebuild query"),
                )
                .on_hover_text("Ask the local model to plan, then replace the gallery filter")
                .clicked()
            {
                outcome.ai_apply = Some(AiSearchApply {
                    query: parsed.query.clone(),
                    confidence,
                    prompt: ai_search.prompt.clone(),
                    scope,
                    mode: AiApplyMode::Rebuild,
                });
            }
            if ui
                .add_enabled(
                    parsed.query != Query::AlwaysTrue && *gallery_query != Query::AlwaysTrue,
                    egui::Button::new("Add to query"),
                )
                .on_hover_text("Narrow the current gallery filter with this query")
                .clicked()
            {
                outcome.ai_apply = Some(AiSearchApply {
                    query: parsed.query.clone(),
                    confidence,
                    prompt: ai_search.prompt.clone(),
                    scope,
                    mode: AiApplyMode::Add,
                });
            }
        });
    });
    ui.separator();

    let metadata_path = if item_id.is_empty() {
        None
    } else {
        Some(Path::new(&item_id))
    };
    outcome.review = draw_tag_review(
        ui,
        review,
        &item_id,
        staged_sidecar,
        metadata_path,
        review_ui,
    );
}

/// Draw the native Tag Manager window. `state.open` tells the caller whether
/// the window remains open; the outcome is still returned on the closing frame.
#[must_use]
pub fn show(
    ctx: &egui::Context,
    state: &mut TagManagerState,
    review: &mut TagReviewStore,
    review_ui: &mut ReviewUiState,
    staged_sidecar: &mut Option<Sidecar>,
    tag_tree: &TagNode,
    gallery_query: &Query,
) -> TagManagerOutcome {
    let mut outcome = TagManagerOutcome::default();
    if !state.open {
        return outcome;
    }
    let builder = ViewportBuilder::default()
        .with_title("Tag Manager")
        .with_inner_size([1100.0, 800.0]);

    ctx.show_viewport_immediate(viewport_id(), builder, |ctx, class| {
        if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
            state.open = false;
        }
        if class == ViewportClass::Embedded {
            let mut embedded_open = state.open;
            egui::Window::new("Tag Manager")
                .open(&mut embedded_open)
                .show(ctx, |ui| {
                    draw_contents(
                        ui,
                        state,
                        review,
                        review_ui,
                        staged_sidecar,
                        tag_tree,
                        gallery_query,
                        &mut outcome,
                    );
                });
            state.open = embedded_open && state.open;
        } else {
            egui::CentralPanel::default().show(ctx, |ui| {
                draw_contents(
                    ui,
                    state,
                    review,
                    review_ui,
                    staged_sidecar,
                    tag_tree,
                    gallery_query,
                    &mut outcome,
                );
            });
        }
    });

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_starts_closed_without_an_item() {
        let state = TagManagerState::default();
        assert!(!state.open);
        assert_eq!(state.item, None);
        assert!(state.targets.entries.is_empty());
    }

    #[test]
    fn target_labels_use_only_the_file_name() {
        assert_eq!(short_name("/library/photos/fox.webp"), "fox.webp");
        assert_eq!(short_name("relative.webp"), "relative.webp");
    }

    #[test]
    fn target_keys_are_precomputed_for_filtering() {
        let mut picker = TargetPickerState::default();
        picker.set_paths(vec![
            "/library/photos/Fox.webp".to_string(),
            "/library/photos/fox2.webp".to_string(),
        ]);
        assert_eq!(picker.entries[0].key, "/library/photos/fox.webp");
        assert!(picker.entries[1].key.contains("fox2.webp"));
    }

    #[test]
    fn target_search_filters_case_insensitively() {
        let mut picker = TargetPickerState::default();
        picker.set_paths(vec![
            "/library/photos/thunderclan.webp".to_string(),
            "/library/photos/fox.webp".to_string(),
        ]);
        assert_eq!(picker.visible(), &[0, 1]);

        picker.search = "THUNDER".to_string();
        assert_eq!(picker.visible(), &[0]);

        picker.search = "fox".to_string();
        assert_eq!(picker.visible(), &[1]);
    }

    #[test]
    fn ai_prompt_starts_with_known_image_data_enabled() {
        let outcome = TagManagerOutcome::default();
        let state = TagManagerState::default();
        assert!(state.ai_search.prompt.is_empty());
        assert!(state.ai_search.include_human);
        assert!(outcome.ai_apply.is_none());
    }
}
