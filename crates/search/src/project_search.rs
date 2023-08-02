use crate::{
    SearchOptions, SelectNextMatch, SelectPrevMatch, ToggleCaseSensitive, ToggleRegex,
    ToggleWholeWord,
};
use anyhow::Result;
use collections::HashMap;
use editor::{
    items::active_match_index, scroll::autoscroll::Autoscroll, Anchor, Editor, MultiBuffer,
    SelectAll, MAX_TAB_TITLE_LEN,
};
use futures::StreamExt;
use globset::{Glob, GlobMatcher};
use gpui::color::Color;
use gpui::geometry::rect::RectF;
use gpui::geometry::vector::IntoVector2F;
use gpui::json::{self, ToJson};
use gpui::SceneBuilder;
use gpui::{
    actions,
    elements::*,
    platform::{CursorStyle, MouseButton},
    Action, AnyElement, AnyViewHandle, AppContext, Entity, ModelContext, ModelHandle, Subscription,
    Task, View, ViewContext, ViewHandle, WeakModelHandle, WeakViewHandle,
};
use gpui::{scene::Path, Border, LayoutContext};
use menu::Confirm;
use postage::stream::Stream;
use project::{search::SearchQuery, Entry, Project};
use semantic_index::SemanticIndex;
use smallvec::SmallVec;
use std::{
    any::{Any, TypeId},
    borrow::Cow,
    collections::HashSet,
    mem,
    ops::{Not, Range},
    path::PathBuf,
    sync::Arc,
};
use util::ResultExt as _;
use workspace::{
    item::{BreadcrumbText, Item, ItemEvent, ItemHandle},
    searchable::{Direction, SearchableItem, SearchableItemHandle},
    ItemNavHistory, Pane, ToolbarItemLocation, ToolbarItemView, Workspace, WorkspaceId,
};

actions!(
    project_search,
    [
        SearchInNew,
        ToggleFocus,
        NextField,
        ToggleSemanticSearch,
        CycleMode
    ]
);

#[derive(Default)]
struct ActiveSearches(HashMap<WeakModelHandle<Project>, WeakViewHandle<ProjectSearchView>>);

pub fn init(cx: &mut AppContext) {
    cx.set_global(ActiveSearches::default());
    cx.add_action(ProjectSearchView::deploy);
    cx.add_action(ProjectSearchView::move_focus_to_results);
    cx.add_action(ProjectSearchBar::search);
    cx.add_action(ProjectSearchBar::search_in_new);
    cx.add_action(ProjectSearchBar::select_next_match);
    cx.add_action(ProjectSearchBar::select_prev_match);
    cx.add_action(ProjectSearchBar::cycle_mode);
    cx.capture_action(ProjectSearchBar::tab);
    cx.capture_action(ProjectSearchBar::tab_previous);
    add_toggle_option_action::<ToggleCaseSensitive>(SearchOptions::CASE_SENSITIVE, cx);
    add_toggle_option_action::<ToggleWholeWord>(SearchOptions::WHOLE_WORD, cx);
    add_toggle_option_action::<ToggleRegex>(SearchOptions::REGEX, cx);
}

fn add_toggle_option_action<A: Action>(option: SearchOptions, cx: &mut AppContext) {
    cx.add_action(move |pane: &mut Pane, _: &A, cx: &mut ViewContext<Pane>| {
        if let Some(search_bar) = pane.toolbar().read(cx).item_of_type::<ProjectSearchBar>() {
            if search_bar.update(cx, |search_bar, cx| {
                search_bar.toggle_search_option(option, cx)
            }) {
                return;
            }
        }
        cx.propagate_action();
    });
}

struct ProjectSearch {
    project: ModelHandle<Project>,
    excerpts: ModelHandle<MultiBuffer>,
    pending_search: Option<Task<Option<()>>>,
    match_ranges: Vec<Range<Anchor>>,
    active_query: Option<SearchQuery>,
    search_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum InputPanel {
    Query,
    Exclude,
    Include,
}

pub struct ProjectSearchView {
    model: ModelHandle<ProjectSearch>,
    query_editor: ViewHandle<Editor>,
    results_editor: ViewHandle<Editor>,
    semantic: Option<SemanticSearchState>,
    search_options: SearchOptions,
    panels_with_errors: HashSet<InputPanel>,
    active_match_index: Option<usize>,
    search_id: usize,
    query_editor_was_focused: bool,
    included_files_editor: ViewHandle<Editor>,
    excluded_files_editor: ViewHandle<Editor>,
    filters_enabled: bool,
    current_mode: SearchMode,
}

struct SemanticSearchState {
    file_count: usize,
    outstanding_file_count: usize,
    _progress_task: Task<()>,
}

// TODO: Update the default search mode to get from config
#[derive(Copy, Clone, Default, PartialEq)]
enum SearchMode {
    #[default]
    Text,
    Semantic,
    Regex,
}

pub struct ProjectSearchBar {
    active_project_search: Option<ViewHandle<ProjectSearchView>>,
    subscription: Option<Subscription>,
}

impl Entity for ProjectSearch {
    type Event = ();
}

impl ProjectSearch {
    fn new(project: ModelHandle<Project>, cx: &mut ModelContext<Self>) -> Self {
        let replica_id = project.read(cx).replica_id();
        Self {
            project,
            excerpts: cx.add_model(|_| MultiBuffer::new(replica_id)),
            pending_search: Default::default(),
            match_ranges: Default::default(),
            active_query: None,
            search_id: 0,
        }
    }

    fn clone(&self, cx: &mut ModelContext<Self>) -> ModelHandle<Self> {
        cx.add_model(|cx| Self {
            project: self.project.clone(),
            excerpts: self
                .excerpts
                .update(cx, |excerpts, cx| cx.add_model(|cx| excerpts.clone(cx))),
            pending_search: Default::default(),
            match_ranges: self.match_ranges.clone(),
            active_query: self.active_query.clone(),
            search_id: self.search_id,
        })
    }

    fn search(&mut self, query: SearchQuery, cx: &mut ModelContext<Self>) {
        let search = self
            .project
            .update(cx, |project, cx| project.search(query.clone(), cx));
        self.search_id += 1;
        self.active_query = Some(query);
        self.match_ranges.clear();
        self.pending_search = Some(cx.spawn_weak(|this, mut cx| async move {
            let matches = search.await.log_err()?;
            let this = this.upgrade(&cx)?;
            let mut matches = matches.into_iter().collect::<Vec<_>>();
            let (_task, mut match_ranges) = this.update(&mut cx, |this, cx| {
                this.match_ranges.clear();
                matches.sort_by_key(|(buffer, _)| buffer.read(cx).file().map(|file| file.path()));
                this.excerpts.update(cx, |excerpts, cx| {
                    excerpts.clear(cx);
                    excerpts.stream_excerpts_with_context_lines(matches, 1, cx)
                })
            });

            while let Some(match_range) = match_ranges.next().await {
                this.update(&mut cx, |this, cx| {
                    this.match_ranges.push(match_range);
                    while let Ok(Some(match_range)) = match_ranges.try_next() {
                        this.match_ranges.push(match_range);
                    }
                    cx.notify();
                });
            }

            this.update(&mut cx, |this, cx| {
                this.pending_search.take();
                cx.notify();
            });

            None
        }));
        cx.notify();
    }

    fn semantic_search(
        &mut self,
        query: String,
        include_files: Vec<GlobMatcher>,
        exclude_files: Vec<GlobMatcher>,
        cx: &mut ModelContext<Self>,
    ) {
        let search = SemanticIndex::global(cx).map(|index| {
            index.update(cx, |semantic_index, cx| {
                semantic_index.search_project(
                    self.project.clone(),
                    query.clone(),
                    10,
                    include_files,
                    exclude_files,
                    cx,
                )
            })
        });
        self.search_id += 1;
        self.match_ranges.clear();
        self.pending_search = Some(cx.spawn(|this, mut cx| async move {
            let results = search?.await.log_err()?;

            let (_task, mut match_ranges) = this.update(&mut cx, |this, cx| {
                this.excerpts.update(cx, |excerpts, cx| {
                    excerpts.clear(cx);

                    let matches = results
                        .into_iter()
                        .map(|result| (result.buffer, vec![result.range.start..result.range.start]))
                        .collect();

                    excerpts.stream_excerpts_with_context_lines(matches, 3, cx)
                })
            });

            while let Some(match_range) = match_ranges.next().await {
                this.update(&mut cx, |this, cx| {
                    this.match_ranges.push(match_range);
                    while let Ok(Some(match_range)) = match_ranges.try_next() {
                        this.match_ranges.push(match_range);
                    }
                    cx.notify();
                });
            }

            this.update(&mut cx, |this, cx| {
                this.pending_search.take();
                cx.notify();
            });

            None
        }));
        cx.notify();
    }
}

pub enum ViewEvent {
    UpdateTab,
    Activate,
    EditorEvent(editor::Event),
}

impl Entity for ProjectSearchView {
    type Event = ViewEvent;
}

impl View for ProjectSearchView {
    fn ui_name() -> &'static str {
        "ProjectSearchView"
    }

    fn render(&mut self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        let model = &self.model.read(cx);
        if model.match_ranges.is_empty() {
            enum Status {}

            let theme = theme::current(cx).clone();

            // If Search is Active -> Major: Searching..., Minor: None
            // If Semantic -> Major: "Search using Natural Language", Minor: {Status}/n{ex...}/n{ex...}
            // If Regex -> Major: "Search using Regex", Minor: {ex...}
            // If Text -> Major: "Text search all files and folders", Minor: {...}

            let current_mode = self.current_mode;
            let major_text = if model.pending_search.is_some() {
                Cow::Borrowed("Searching...")
            } else {
                match current_mode {
                    SearchMode::Text => Cow::Borrowed("Text search all files and folders"),
                    SearchMode::Semantic => {
                        Cow::Borrowed("Search all files and folders using Natural Language")
                    }
                    SearchMode::Regex => Cow::Borrowed("Regex search all files and folders"),
                }
            };

            let semantic_status = if let Some(semantic) = &self.semantic {
                if semantic.outstanding_file_count > 0 {
                    let dots_count = semantic.outstanding_file_count % 3 + 1;
                    let dots: String = std::iter::repeat('.').take(dots_count).collect();
                    format!(
                        "Indexing. {} of {}{dots}",
                        semantic.file_count - semantic.outstanding_file_count,
                        semantic.file_count
                    )
                } else {
                    "Indexing complete".to_string()
                }
            } else {
                "This is an invalid state".to_string()
            };

            let minor_text = match current_mode {
                SearchMode::Semantic => [
                    semantic_status,
                    "ex. list all available languages".to_owned(),
                ],
                _ => [
                    "Include/exclude specific paths with the filter option.".to_owned(),
                    "Matching exact word and/or casing is available too.".to_owned(),
                ],
            };

            MouseEventHandler::<Status, _>::new(0, cx, |_, _| {
                Flex::column()
                    .with_child(Flex::column().contained().flex(1., true))
                    .with_child(
                        Flex::column()
                            .align_children_center()
                            .with_child(Label::new(
                                major_text,
                                theme.search.major_results_status.clone(),
                            ))
                            .with_children(
                                minor_text.into_iter().map(|x| {
                                    Label::new(x, theme.search.minor_results_status.clone())
                                }),
                            )
                            .aligned()
                            .top()
                            .contained()
                            .flex(7., true),
                    )
                    .contained()
                    .with_background_color(theme.editor.background)
            })
            .on_down(MouseButton::Left, |_, _, cx| {
                cx.focus_parent();
            })
            .into_any_named("project search view")
        } else {
            ChildView::new(&self.results_editor, cx)
                .flex(1., true)
                .into_any_named("project search view")
        }
    }

    fn focus_in(&mut self, _: AnyViewHandle, cx: &mut ViewContext<Self>) {
        let handle = cx.weak_handle();
        cx.update_global(|state: &mut ActiveSearches, cx| {
            state
                .0
                .insert(self.model.read(cx).project.downgrade(), handle)
        });

        if cx.is_self_focused() {
            if self.query_editor_was_focused {
                cx.focus(&self.query_editor);
            } else {
                cx.focus(&self.results_editor);
            }
        }
    }
}

impl Item for ProjectSearchView {
    fn tab_tooltip_text(&self, cx: &AppContext) -> Option<Cow<str>> {
        let query_text = self.query_editor.read(cx).text(cx);

        query_text
            .is_empty()
            .not()
            .then(|| query_text.into())
            .or_else(|| Some("Project Search".into()))
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a ViewHandle<Self>,
        _: &'a AppContext,
    ) -> Option<&'a AnyViewHandle> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle)
        } else if type_id == TypeId::of::<Editor>() {
            Some(&self.results_editor)
        } else {
            None
        }
    }

    fn deactivated(&mut self, cx: &mut ViewContext<Self>) {
        self.results_editor
            .update(cx, |editor, cx| editor.deactivated(cx));
    }

    fn tab_content<T: View>(
        &self,
        _detail: Option<usize>,
        tab_theme: &theme::Tab,
        cx: &AppContext,
    ) -> AnyElement<T> {
        Flex::row()
            .with_child(
                Svg::new("icons/magnifying_glass_12.svg")
                    .with_color(tab_theme.label.text.color)
                    .constrained()
                    .with_width(tab_theme.type_icon_width)
                    .aligned()
                    .contained()
                    .with_margin_right(tab_theme.spacing),
            )
            .with_child({
                let tab_name: Option<Cow<_>> =
                    self.model.read(cx).active_query.as_ref().map(|query| {
                        let query_text =
                            util::truncate_and_trailoff(query.as_str(), MAX_TAB_TITLE_LEN);
                        query_text.into()
                    });
                Label::new(
                    tab_name
                        .filter(|name| !name.is_empty())
                        .unwrap_or("Project search".into()),
                    tab_theme.label.clone(),
                )
                .aligned()
            })
            .into_any()
    }

    fn for_each_project_item(&self, cx: &AppContext, f: &mut dyn FnMut(usize, &dyn project::Item)) {
        self.results_editor.for_each_project_item(cx, f)
    }

    fn is_singleton(&self, _: &AppContext) -> bool {
        false
    }

    fn can_save(&self, _: &AppContext) -> bool {
        true
    }

    fn is_dirty(&self, cx: &AppContext) -> bool {
        self.results_editor.read(cx).is_dirty(cx)
    }

    fn has_conflict(&self, cx: &AppContext) -> bool {
        self.results_editor.read(cx).has_conflict(cx)
    }

    fn save(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.results_editor
            .update(cx, |editor, cx| editor.save(project, cx))
    }

    fn save_as(
        &mut self,
        _: ModelHandle<Project>,
        _: PathBuf,
        _: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        unreachable!("save_as should not have been called")
    }

    fn reload(
        &mut self,
        project: ModelHandle<Project>,
        cx: &mut ViewContext<Self>,
    ) -> Task<anyhow::Result<()>> {
        self.results_editor
            .update(cx, |editor, cx| editor.reload(project, cx))
    }

    fn clone_on_split(&self, _workspace_id: WorkspaceId, cx: &mut ViewContext<Self>) -> Option<Self>
    where
        Self: Sized,
    {
        let model = self.model.update(cx, |model, cx| model.clone(cx));
        Some(Self::new(model, cx))
    }

    fn added_to_workspace(&mut self, workspace: &mut Workspace, cx: &mut ViewContext<Self>) {
        self.results_editor
            .update(cx, |editor, cx| editor.added_to_workspace(workspace, cx));
    }

    fn set_nav_history(&mut self, nav_history: ItemNavHistory, cx: &mut ViewContext<Self>) {
        self.results_editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn navigate(&mut self, data: Box<dyn Any>, cx: &mut ViewContext<Self>) -> bool {
        self.results_editor
            .update(cx, |editor, cx| editor.navigate(data, cx))
    }

    fn to_item_events(event: &Self::Event) -> SmallVec<[ItemEvent; 2]> {
        match event {
            ViewEvent::UpdateTab => {
                smallvec::smallvec![ItemEvent::UpdateBreadcrumbs, ItemEvent::UpdateTab]
            }
            ViewEvent::EditorEvent(editor_event) => Editor::to_item_events(editor_event),
            _ => SmallVec::new(),
        }
    }

    fn breadcrumb_location(&self) -> ToolbarItemLocation {
        if self.has_matches() {
            ToolbarItemLocation::Secondary
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, theme: &theme::Theme, cx: &AppContext) -> Option<Vec<BreadcrumbText>> {
        self.results_editor.breadcrumbs(theme, cx)
    }

    fn serialized_item_kind() -> Option<&'static str> {
        None
    }

    fn deserialize(
        _project: ModelHandle<Project>,
        _workspace: WeakViewHandle<Workspace>,
        _workspace_id: workspace::WorkspaceId,
        _item_id: workspace::ItemId,
        _cx: &mut ViewContext<Pane>,
    ) -> Task<anyhow::Result<ViewHandle<Self>>> {
        unimplemented!()
    }
}

impl ProjectSearchView {
    fn new(model: ModelHandle<ProjectSearch>, cx: &mut ViewContext<Self>) -> Self {
        let project;
        let excerpts;
        let mut query_text = String::new();
        let mut options = SearchOptions::NONE;

        {
            let model = model.read(cx);
            project = model.project.clone();
            excerpts = model.excerpts.clone();
            if let Some(active_query) = model.active_query.as_ref() {
                query_text = active_query.as_str().to_string();
                options = SearchOptions::from_query(active_query);
            }
        }
        cx.observe(&model, |this, _, cx| this.model_changed(cx))
            .detach();

        let query_editor = cx.add_view(|cx| {
            let mut editor = Editor::single_line(
                Some(Arc::new(|theme| theme.search.editor.input.clone())),
                cx,
            );
            editor.set_placeholder_text("Text search all files", cx);
            editor.set_text(query_text, cx);
            editor
        });
        // Subscribe to query_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&query_editor, |_, _, event, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();

        let results_editor = cx.add_view(|cx| {
            let mut editor = Editor::for_multibuffer(excerpts, Some(project), cx);
            editor.set_searchable(false);
            editor
        });
        cx.observe(&results_editor, |_, _, cx| cx.emit(ViewEvent::UpdateTab))
            .detach();

        cx.subscribe(&results_editor, |this, _, event, cx| {
            if matches!(event, editor::Event::SelectionsChanged { .. }) {
                this.update_match_index(cx);
            }
            // Reraise editor events for workspace item activation purposes
            cx.emit(ViewEvent::EditorEvent(event.clone()));
        })
        .detach();

        let included_files_editor = cx.add_view(|cx| {
            let mut editor = Editor::single_line(
                Some(Arc::new(|theme| {
                    theme.search.include_exclude_editor.input.clone()
                })),
                cx,
            );
            editor.set_placeholder_text("Include: crates/**/*.toml", cx);

            editor
        });
        // Subscribe to include_files_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&included_files_editor, |_, _, event, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();

        let excluded_files_editor = cx.add_view(|cx| {
            let mut editor = Editor::single_line(
                Some(Arc::new(|theme| {
                    theme.search.include_exclude_editor.input.clone()
                })),
                cx,
            );
            editor.set_placeholder_text("Exclude: vendor/*, *.lock", cx);

            editor
        });
        // Subscribe to excluded_files_editor in order to reraise editor events for workspace item activation purposes
        cx.subscribe(&excluded_files_editor, |_, _, event, cx| {
            cx.emit(ViewEvent::EditorEvent(event.clone()))
        })
        .detach();
        let filters_enabled = false;
        let mut this = ProjectSearchView {
            search_id: model.read(cx).search_id,
            model,
            query_editor,
            results_editor,
            semantic: None,
            search_options: options,
            panels_with_errors: HashSet::new(),
            active_match_index: None,
            query_editor_was_focused: false,
            included_files_editor,
            excluded_files_editor,
            filters_enabled,
            current_mode: Default::default(),
        };
        this.model_changed(cx);
        this
    }

    pub fn new_search_in_directory(
        workspace: &mut Workspace,
        dir_entry: &Entry,
        cx: &mut ViewContext<Workspace>,
    ) {
        if !dir_entry.is_dir() {
            return;
        }
        let filter_path = dir_entry.path.join("**");
        let Some(filter_str) = filter_path.to_str() else { return; };

        let model = cx.add_model(|cx| ProjectSearch::new(workspace.project().clone(), cx));
        let search = cx.add_view(|cx| ProjectSearchView::new(model, cx));
        workspace.add_item(Box::new(search.clone()), cx);
        search.update(cx, |search, cx| {
            search
                .included_files_editor
                .update(cx, |editor, cx| editor.set_text(filter_str, cx));
            search.focus_query_editor(cx)
        });
    }

    // Re-activate the most recently activated search or the most recent if it has been closed.
    // If no search exists in the workspace, create a new one.
    fn deploy(
        workspace: &mut Workspace,
        _: &workspace::NewSearch,
        cx: &mut ViewContext<Workspace>,
    ) {
        // Clean up entries for dropped projects
        cx.update_global(|state: &mut ActiveSearches, cx| {
            state.0.retain(|project, _| project.is_upgradable(cx))
        });

        let active_search = cx
            .global::<ActiveSearches>()
            .0
            .get(&workspace.project().downgrade());

        let existing = active_search
            .and_then(|active_search| {
                workspace
                    .items_of_type::<ProjectSearchView>(cx)
                    .find(|search| search == active_search)
            })
            .or_else(|| workspace.item_of_type::<ProjectSearchView>(cx));

        let query = workspace.active_item(cx).and_then(|item| {
            let editor = item.act_as::<Editor>(cx)?;
            let query = editor.query_suggestion(cx);
            if query.is_empty() {
                None
            } else {
                Some(query)
            }
        });

        let search = if let Some(existing) = existing {
            workspace.activate_item(&existing, cx);
            existing
        } else {
            let model = cx.add_model(|cx| ProjectSearch::new(workspace.project().clone(), cx));
            let view = cx.add_view(|cx| ProjectSearchView::new(model, cx));
            workspace.add_item(Box::new(view.clone()), cx);
            view
        };

        search.update(cx, |search, cx| {
            if let Some(query) = query {
                search.set_query(&query, cx);
            }
            search.focus_query_editor(cx)
        });
    }

    fn search(&mut self, cx: &mut ViewContext<Self>) {
        if let Some(semantic) = &mut self.semantic {
            if semantic.outstanding_file_count > 0 {
                return;
            }

            let query = self.query_editor.read(cx).text(cx);
            if let Some((included_files, exclude_files)) =
                self.get_included_and_excluded_globsets(cx)
            {
                self.model.update(cx, |model, cx| {
                    model.semantic_search(query, included_files, exclude_files, cx)
                });
            }
            return;
        }

        if let Some(query) = self.build_search_query(cx) {
            self.model.update(cx, |model, cx| model.search(query, cx));
        }
    }

    fn get_included_and_excluded_globsets(
        &mut self,
        cx: &mut ViewContext<Self>,
    ) -> Option<(Vec<GlobMatcher>, Vec<GlobMatcher>)> {
        let included_files =
            match Self::load_glob_set(&self.included_files_editor.read(cx).text(cx)) {
                Ok(included_files) => {
                    self.panels_with_errors.remove(&InputPanel::Include);
                    included_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Include);
                    cx.notify();
                    return None;
                }
            };
        let excluded_files =
            match Self::load_glob_set(&self.excluded_files_editor.read(cx).text(cx)) {
                Ok(excluded_files) => {
                    self.panels_with_errors.remove(&InputPanel::Exclude);
                    excluded_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Exclude);
                    cx.notify();
                    return None;
                }
            };

        Some((included_files, excluded_files))
    }

    fn build_search_query(&mut self, cx: &mut ViewContext<Self>) -> Option<SearchQuery> {
        let text = self.query_editor.read(cx).text(cx);
        let included_files =
            match Self::load_glob_set(&self.included_files_editor.read(cx).text(cx)) {
                Ok(included_files) => {
                    self.panels_with_errors.remove(&InputPanel::Include);
                    included_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Include);
                    cx.notify();
                    return None;
                }
            };
        let excluded_files =
            match Self::load_glob_set(&self.excluded_files_editor.read(cx).text(cx)) {
                Ok(excluded_files) => {
                    self.panels_with_errors.remove(&InputPanel::Exclude);
                    excluded_files
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Exclude);
                    cx.notify();
                    return None;
                }
            };
        if self.search_options.contains(SearchOptions::REGEX) {
            match SearchQuery::regex(
                text,
                self.search_options.contains(SearchOptions::WHOLE_WORD),
                self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                included_files,
                excluded_files,
            ) {
                Ok(query) => {
                    self.panels_with_errors.remove(&InputPanel::Query);
                    Some(query)
                }
                Err(_e) => {
                    self.panels_with_errors.insert(InputPanel::Query);
                    cx.notify();
                    None
                }
            }
        } else {
            Some(SearchQuery::text(
                text,
                self.search_options.contains(SearchOptions::WHOLE_WORD),
                self.search_options.contains(SearchOptions::CASE_SENSITIVE),
                included_files,
                excluded_files,
            ))
        }
    }

    fn load_glob_set(text: &str) -> Result<Vec<GlobMatcher>> {
        text.split(',')
            .map(str::trim)
            .filter(|glob_str| !glob_str.is_empty())
            .map(|glob_str| anyhow::Ok(Glob::new(glob_str)?.compile_matcher()))
            .collect()
    }

    fn select_match(&mut self, direction: Direction, cx: &mut ViewContext<Self>) {
        if let Some(index) = self.active_match_index {
            let match_ranges = self.model.read(cx).match_ranges.clone();
            let new_index = self.results_editor.update(cx, |editor, cx| {
                editor.match_index_for_direction(&match_ranges, index, direction, 1, cx)
            });

            let range_to_select = match_ranges[new_index].clone();
            self.results_editor.update(cx, |editor, cx| {
                editor.unfold_ranges([range_to_select.clone()], false, true, cx);
                editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                    s.select_ranges([range_to_select])
                });
            });
        }
    }

    fn focus_query_editor(&mut self, cx: &mut ViewContext<Self>) {
        self.query_editor.update(cx, |query_editor, cx| {
            query_editor.select_all(&SelectAll, cx);
        });
        self.query_editor_was_focused = true;
        cx.focus(&self.query_editor);
    }

    fn set_query(&mut self, query: &str, cx: &mut ViewContext<Self>) {
        self.query_editor
            .update(cx, |query_editor, cx| query_editor.set_text(query, cx));
    }

    fn focus_results_editor(&mut self, cx: &mut ViewContext<Self>) {
        self.query_editor.update(cx, |query_editor, cx| {
            let cursor = query_editor.selections.newest_anchor().head();
            query_editor.change_selections(None, cx, |s| s.select_ranges([cursor.clone()..cursor]));
        });
        self.query_editor_was_focused = false;
        cx.focus(&self.results_editor);
    }

    fn model_changed(&mut self, cx: &mut ViewContext<Self>) {
        let match_ranges = self.model.read(cx).match_ranges.clone();
        if match_ranges.is_empty() {
            self.active_match_index = None;
        } else {
            self.active_match_index = Some(0);
            self.update_match_index(cx);
            let prev_search_id = mem::replace(&mut self.search_id, self.model.read(cx).search_id);
            let is_new_search = self.search_id != prev_search_id;
            self.results_editor.update(cx, |editor, cx| {
                if is_new_search {
                    editor.change_selections(Some(Autoscroll::fit()), cx, |s| {
                        s.select_ranges(match_ranges.first().cloned())
                    });
                }
                editor.highlight_background::<Self>(
                    match_ranges,
                    |theme| theme.search.match_background,
                    cx,
                );
            });
            if is_new_search && self.query_editor.is_focused(cx) {
                self.focus_results_editor(cx);
            }
        }

        cx.emit(ViewEvent::UpdateTab);
        cx.notify();
    }

    fn update_match_index(&mut self, cx: &mut ViewContext<Self>) {
        let results_editor = self.results_editor.read(cx);
        let new_index = active_match_index(
            &self.model.read(cx).match_ranges,
            &results_editor.selections.newest_anchor().head(),
            &results_editor.buffer().read(cx).snapshot(cx),
        );
        if self.active_match_index != new_index {
            self.active_match_index = new_index;
            cx.notify();
        }
    }

    pub fn has_matches(&self) -> bool {
        self.active_match_index.is_some()
    }

    fn move_focus_to_results(pane: &mut Pane, _: &ToggleFocus, cx: &mut ViewContext<Pane>) {
        if let Some(search_view) = pane
            .active_item()
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            search_view.update(cx, |search_view, cx| {
                if !search_view.results_editor.is_focused(cx)
                    && !search_view.model.read(cx).match_ranges.is_empty()
                {
                    return search_view.focus_results_editor(cx);
                }
            });
        }

        cx.propagate_action();
    }
}

impl Default for ProjectSearchBar {
    fn default() -> Self {
        Self::new()
    }
}
type CreatePath = fn(RectF, Color) -> Path;
type AdjustBorder = fn(RectF, f32) -> RectF;
pub struct ButtonSide {
    color: Color,
    factory: CreatePath,
    border_adjustment: AdjustBorder,
    border: Option<(f32, Color)>,
}

impl ButtonSide {
    fn new(color: Color, factory: CreatePath, border_adjustment: AdjustBorder) -> Self {
        Self {
            color,
            factory,
            border_adjustment,
            border: None,
        }
    }
    pub fn with_border(mut self, width: f32, color: Color) -> Self {
        self.border = Some((width, color));
        self
    }
    pub fn left(color: Color) -> Self {
        Self::new(color, left_button_side, left_button_border_adjust)
    }
    pub fn right(color: Color) -> Self {
        Self::new(color, right_button_side, right_button_border_adjust)
    }
}
fn left_button_border_adjust(bounds: RectF, width: f32) -> RectF {
    let width = width.into_vector_2f();
    let mut lower_right = bounds.clone().lower_right();
    lower_right.set_x(lower_right.x() + width.x());
    RectF::from_points(bounds.origin() + width, lower_right)
}
fn right_button_border_adjust(bounds: RectF, width: f32) -> RectF {
    let width = width.into_vector_2f();
    let mut origin = bounds.clone().origin();
    origin.set_x(origin.x() - width.x());
    RectF::from_points(origin, bounds.lower_right() - width)
}
fn left_button_side(bounds: RectF, color: Color) -> Path {
    use gpui::geometry::PathBuilder;
    let mut path = PathBuilder::new();
    path.reset(bounds.lower_right());
    path.line_to(bounds.upper_right());
    let mut middle_point = bounds.origin();
    let distance_to_line = (middle_point.y() - bounds.lower_left().y()) / 4.;
    middle_point.set_y(middle_point.y() - distance_to_line);
    path.curve_to(middle_point, bounds.origin());
    let mut target = bounds.lower_left();
    target.set_y(target.y() + distance_to_line);
    path.line_to(target);
    path.curve_to(bounds.lower_right(), bounds.lower_left());
    path.build(color, None)
}

fn right_button_side(bounds: RectF, color: Color) -> Path {
    use gpui::geometry::PathBuilder;
    let mut path = PathBuilder::new();
    path.reset(bounds.lower_left());
    path.line_to(bounds.origin());
    let mut middle_point = bounds.upper_right();
    let distance_to_line = (middle_point.y() - bounds.lower_right().y()) / 4.;
    middle_point.set_y(middle_point.y() - distance_to_line);
    path.curve_to(middle_point, bounds.upper_right());
    let mut target = bounds.lower_right();
    target.set_y(target.y() + distance_to_line);
    path.line_to(target);
    path.curve_to(bounds.lower_left(), bounds.lower_right());
    path.build(color, None)
}

impl Element<ProjectSearchBar> for ButtonSide {
    type LayoutState = ();

    type PaintState = ();

    fn layout(
        &mut self,
        constraint: gpui::SizeConstraint,
        _: &mut ProjectSearchBar,
        _: &mut LayoutContext<ProjectSearchBar>,
    ) -> (gpui::geometry::vector::Vector2F, Self::LayoutState) {
        (constraint.max, ())
    }

    fn paint(
        &mut self,
        scene: &mut SceneBuilder,
        bounds: RectF,
        _: RectF,
        _: &mut Self::LayoutState,
        _: &mut ProjectSearchBar,
        _: &mut ViewContext<ProjectSearchBar>,
    ) -> Self::PaintState {
        let mut bounds = bounds;
        if let Some((border_width, border_color)) = self.border.as_ref() {
            scene.push_path((self.factory)(bounds, border_color.clone()));
            bounds = (self.border_adjustment)(bounds, *border_width);
        };
        scene.push_path((self.factory)(bounds, self.color));
    }

    fn rect_for_text_range(
        &self,
        _: Range<usize>,
        _: RectF,
        _: RectF,
        _: &Self::LayoutState,
        _: &Self::PaintState,
        _: &ProjectSearchBar,
        _: &ViewContext<ProjectSearchBar>,
    ) -> Option<RectF> {
        None
    }

    fn debug(
        &self,
        bounds: RectF,
        _: &Self::LayoutState,
        _: &Self::PaintState,
        _: &ProjectSearchBar,
        _: &ViewContext<ProjectSearchBar>,
    ) -> gpui::json::Value {
        json::json!({
            "type": "ButtonSide",
            "bounds": bounds.to_json(),
            "color": self.color.to_json(),
        })
    }
}

impl ProjectSearchBar {
    pub fn new() -> Self {
        Self {
            active_project_search: Default::default(),
            subscription: Default::default(),
        }
    }
    fn cycle_mode(workspace: &mut Workspace, _: &CycleMode, cx: &mut ViewContext<Workspace>) {
        if let Some(search_view) = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            search_view.update(cx, |this, cx| {
                let mode = &this.current_mode;
                let next_text_state = if SemanticIndex::enabled(cx) {
                    SearchMode::Semantic
                } else {
                    SearchMode::Regex
                };

                this.current_mode = match mode {
                    &SearchMode::Text => next_text_state,
                    &SearchMode::Semantic => SearchMode::Regex,
                    SearchMode::Regex => SearchMode::Text,
                };
                cx.notify();
            })
        }
    }
    fn search(&mut self, _: &Confirm, cx: &mut ViewContext<Self>) {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| search_view.search(cx));
        }
    }

    fn search_in_new(workspace: &mut Workspace, _: &SearchInNew, cx: &mut ViewContext<Workspace>) {
        if let Some(search_view) = workspace
            .active_item(cx)
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            let new_query = search_view.update(cx, |search_view, cx| {
                let new_query = search_view.build_search_query(cx);
                if new_query.is_some() {
                    if let Some(old_query) = search_view.model.read(cx).active_query.clone() {
                        search_view.query_editor.update(cx, |editor, cx| {
                            editor.set_text(old_query.as_str(), cx);
                        });
                        search_view.search_options = SearchOptions::from_query(&old_query);
                    }
                }
                new_query
            });
            if let Some(new_query) = new_query {
                let model = cx.add_model(|cx| {
                    let mut model = ProjectSearch::new(workspace.project().clone(), cx);
                    model.search(new_query, cx);
                    model
                });
                workspace.add_item(
                    Box::new(cx.add_view(|cx| ProjectSearchView::new(model, cx))),
                    cx,
                );
            }
        }
    }

    fn select_next_match(pane: &mut Pane, _: &SelectNextMatch, cx: &mut ViewContext<Pane>) {
        if let Some(search_view) = pane
            .active_item()
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            search_view.update(cx, |view, cx| view.select_match(Direction::Next, cx));
        } else {
            cx.propagate_action();
        }
    }

    fn select_prev_match(pane: &mut Pane, _: &SelectPrevMatch, cx: &mut ViewContext<Pane>) {
        if let Some(search_view) = pane
            .active_item()
            .and_then(|item| item.downcast::<ProjectSearchView>())
        {
            search_view.update(cx, |view, cx| view.select_match(Direction::Prev, cx));
        } else {
            cx.propagate_action();
        }
    }

    fn tab(&mut self, _: &editor::Tab, cx: &mut ViewContext<Self>) {
        self.cycle_field(Direction::Next, cx);
    }

    fn tab_previous(&mut self, _: &editor::TabPrev, cx: &mut ViewContext<Self>) {
        self.cycle_field(Direction::Prev, cx);
    }

    fn cycle_field(&mut self, direction: Direction, cx: &mut ViewContext<Self>) {
        let active_project_search = match &self.active_project_search {
            Some(active_project_search) => active_project_search,

            None => {
                cx.propagate_action();
                return;
            }
        };

        active_project_search.update(cx, |project_view, cx| {
            let views = &[
                &project_view.query_editor,
                &project_view.included_files_editor,
                &project_view.excluded_files_editor,
            ];

            let current_index = match views
                .iter()
                .enumerate()
                .find(|(_, view)| view.is_focused(cx))
            {
                Some((index, _)) => index,

                None => {
                    cx.propagate_action();
                    return;
                }
            };

            let new_index = match direction {
                Direction::Next => (current_index + 1) % views.len(),
                Direction::Prev if current_index == 0 => views.len() - 1,
                Direction::Prev => (current_index - 1) % views.len(),
            };
            cx.focus(views[new_index]);
        });
    }

    fn toggle_search_option(&mut self, option: SearchOptions, cx: &mut ViewContext<Self>) -> bool {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                search_view.search_options.toggle(option);
                if option.contains(SearchOptions::REGEX) {
                    search_view.current_mode = SearchMode::Regex;
                }
                search_view.semantic = None;
                search_view.search(cx);
            });
            cx.notify();
            true
        } else {
            false
        }
    }
    fn toggle_filters(&mut self, cx: &mut ViewContext<Self>) -> bool {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                search_view.filters_enabled = !search_view.filters_enabled;
                search_view
                    .included_files_editor
                    .update(cx, |_, cx| cx.notify());
                search_view
                    .excluded_files_editor
                    .update(cx, |_, cx| cx.notify());
                search_view.semantic = None;
                search_view.search(cx);
                cx.notify();
            });
            cx.notify();
            true
        } else {
            false
        }
    }

    fn toggle_semantic_search(&mut self, cx: &mut ViewContext<Self>) -> bool {
        if let Some(search_view) = self.active_project_search.as_ref() {
            search_view.update(cx, |search_view, cx| {
                if search_view.semantic.is_some() {
                    search_view.semantic = None;
                } else if let Some(semantic_index) = SemanticIndex::global(cx) {
                    search_view.current_mode = SearchMode::Semantic;
                    // TODO: confirm that it's ok to send this project
                    search_view.search_options = SearchOptions::none();

                    let project = search_view.model.read(cx).project.clone();
                    let index_task = semantic_index.update(cx, |semantic_index, cx| {
                        semantic_index.index_project(project, cx)
                    });

                    cx.spawn(|search_view, mut cx| async move {
                        let (files_to_index, mut files_remaining_rx) = index_task.await?;

                        search_view.update(&mut cx, |search_view, cx| {
                            cx.notify();
                            search_view.semantic = Some(SemanticSearchState {
                                file_count: files_to_index,
                                outstanding_file_count: files_to_index,
                                _progress_task: cx.spawn(|search_view, mut cx| async move {
                                    while let Some(count) = files_remaining_rx.recv().await {
                                        search_view
                                            .update(&mut cx, |search_view, cx| {
                                                if let Some(semantic_search_state) =
                                                    &mut search_view.semantic
                                                {
                                                    semantic_search_state.outstanding_file_count =
                                                        count;
                                                    cx.notify();
                                                    if count == 0 {
                                                        return;
                                                    }
                                                }
                                            })
                                            .ok();
                                    }
                                }),
                            });
                        })?;
                        anyhow::Ok(())
                    })
                    .detach_and_log_err(cx);
                }
                cx.notify();
            });
            cx.notify();
            true
        } else {
            false
        }
    }

    fn render_nav_button(
        &self,
        icon: &'static str,
        direction: Direction,
        cx: &mut ViewContext<Self>,
    ) -> AnyElement<Self> {
        let action: Box<dyn Action>;
        let tooltip;
        match direction {
            Direction::Prev => {
                action = Box::new(SelectPrevMatch);
                tooltip = "Select Previous Match";
            }
            Direction::Next => {
                action = Box::new(SelectNextMatch);
                tooltip = "Select Next Match";
            }
        };
        let tooltip_style = theme::current(cx).tooltip.clone();

        enum NavButton {}
        MouseEventHandler::<NavButton, _>::new(direction as usize, cx, |state, cx| {
            let theme = theme::current(cx);
            let style = theme.search.option_button.inactive_state().style_for(state);
            Label::new(icon, style.text.clone())
                .contained()
                .with_style(style.container)
        })
        .on_click(MouseButton::Left, move |_, this, cx| {
            if let Some(search) = this.active_project_search.as_ref() {
                search.update(cx, |search, cx| search.select_match(direction, cx));
            }
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<NavButton>(
            direction as usize,
            tooltip.to_string(),
            Some(action),
            tooltip_style,
            cx,
        )
        .into_any()
    }
    fn render_option_button_icon(
        &self,
        icon: &'static str,
        option: SearchOptions,
        cx: &mut ViewContext<Self>,
    ) -> AnyElement<Self> {
        let tooltip_style = theme::current(cx).tooltip.clone();
        let is_active = self.is_option_enabled(option, cx);
        MouseEventHandler::<Self, _>::new(option.bits as usize, cx, |state, cx| {
            let theme = theme::current(cx);
            let style = theme
                .search
                .option_button
                .in_state(is_active)
                .style_for(state);
            Svg::new(icon)
                .with_color(style.text.color.clone())
                .contained()
                .with_style(style.container)
                .constrained()
        })
        .on_click(MouseButton::Left, move |_, this, cx| {
            this.toggle_search_option(option, cx);
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<Self>(
            option.bits as usize,
            format!("Toggle {}", option.label()),
            Some(option.to_toggle_action()),
            tooltip_style,
            cx,
        )
        .into_any()
    }

    fn render_regex_button(
        &self,
        icon: &'static str,
        current_mode: SearchMode,
        cx: &mut ViewContext<Self>,
    ) -> AnyElement<Self> {
        let tooltip_style = theme::current(cx).tooltip.clone();
        let is_active = current_mode == SearchMode::Regex; //self.is_option_enabled(option, cx);
        let option = SearchOptions::REGEX;
        MouseEventHandler::<Self, _>::new(option.bits as usize, cx, |state, cx| {
            let theme = theme::current(cx);
            let mut style = theme
                .search
                .option_button
                .in_state(is_active)
                .style_for(state)
                .clone();
            style.container.border.right = false;

            Flex::row()
                .with_child(
                    Label::new(icon, style.text.clone())
                        .contained()
                        .with_style(style.container),
                )
                .with_child(
                    ButtonSide::right(
                        style
                            .container
                            .background_color
                            .unwrap_or_else(gpui::color::Color::transparent_black),
                    )
                    .with_border(style.container.border.width, style.container.border.color)
                    .contained()
                    .constrained()
                    .with_max_width(theme.titlebar.avatar_ribbon.width / 2.)
                    .aligned()
                    .bottom(),
                )
        })
        .on_click(MouseButton::Left, move |_, this, cx| {
            this.toggle_search_option(option, cx);
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<Self>(
            option.bits as usize,
            format!("Toggle {}", option.label()),
            Some(option.to_toggle_action()),
            tooltip_style,
            cx,
        )
        .into_any()
    }

    fn render_semantic_search_button(&self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        let tooltip_style = theme::current(cx).tooltip.clone();
        let is_active = if let Some(search) = self.active_project_search.as_ref() {
            let search = search.read(cx);
            search.current_mode == SearchMode::Semantic
        } else {
            false
        };

        let region_id = 3;

        MouseEventHandler::<Self, _>::new(region_id, cx, |state, cx| {
            let theme = theme::current(cx);
            let style = theme
                .search
                .option_button
                .in_state(is_active)
                .style_for(state);
            Label::new("Semantic", style.text.clone())
                .contained()
                .with_style(style.container)
        })
        .on_click(MouseButton::Left, move |_, this, cx| {
            this.toggle_semantic_search(cx);
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<Self>(
            region_id,
            format!("Toggle Semantic Search"),
            Some(Box::new(ToggleSemanticSearch)),
            tooltip_style,
            cx,
        )
        .into_any()
    }
    fn render_text_search_button(&self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        let tooltip_style = theme::current(cx).tooltip.clone();
        let is_active = if let Some(search) = self.active_project_search.as_ref() {
            let search = search.read(cx);
            search.current_mode == SearchMode::Text
        } else {
            false
        };

        let region_id = 4;
        enum NormalSearchTag {}
        MouseEventHandler::<NormalSearchTag, _>::new(region_id, cx, |state, cx| {
            let theme = theme::current(cx);
            let mut style = theme
                .search
                .option_button
                .in_state(is_active)
                .style_for(state)
                .clone();
            style.container.border.left = false;
            Flex::row()
                .with_child(
                    ButtonSide::left(
                        style
                            .container
                            .background_color
                            .unwrap_or_else(gpui::color::Color::transparent_black),
                    )
                    .with_border(style.container.border.width, style.container.border.color)
                    .contained()
                    .constrained()
                    .with_max_width(theme.titlebar.avatar_ribbon.width / 2.)
                    .aligned()
                    .bottom(),
                )
                .with_child(
                    Label::new("Text", style.text.clone())
                        .contained()
                        .with_style(style.container),
                )
        })
        .on_click(MouseButton::Left, move |_, this, cx| {
            if let Some(search) = this.active_project_search.as_mut() {
                search.update(cx, |this, cx| {
                    this.semantic = None;
                    this.current_mode = SearchMode::Text;
                    cx.notify();
                });
            }
        })
        .with_cursor_style(CursorStyle::PointingHand)
        .with_tooltip::<NormalSearchTag>(
            region_id,
            format!("Toggle Normal Search"),
            None,
            tooltip_style,
            cx,
        )
        .into_any()
    }
    fn is_option_enabled(&self, option: SearchOptions, cx: &AppContext) -> bool {
        if let Some(search) = self.active_project_search.as_ref() {
            search.read(cx).search_options.contains(option)
        } else {
            false
        }
    }
}

impl Entity for ProjectSearchBar {
    type Event = ();
}

impl View for ProjectSearchBar {
    fn ui_name() -> &'static str {
        "ProjectSearchBar"
    }

    fn render(&mut self, cx: &mut ViewContext<Self>) -> AnyElement<Self> {
        if let Some(_search) = self.active_project_search.as_ref() {
            let search = _search.read(cx);
            let theme = theme::current(cx).clone();
            let query_container_style = if search.panels_with_errors.contains(&InputPanel::Query) {
                theme.search.invalid_editor
            } else {
                theme.search.editor.input.container
            };
            let include_container_style =
                if search.panels_with_errors.contains(&InputPanel::Include) {
                    theme.search.invalid_include_exclude_editor
                } else {
                    theme.search.include_exclude_editor.input.container
                };
            let exclude_container_style =
                if search.panels_with_errors.contains(&InputPanel::Exclude) {
                    theme.search.invalid_include_exclude_editor
                } else {
                    theme.search.include_exclude_editor.input.container
                };

            let included_files_view = ChildView::new(&search.included_files_editor, cx)
                .aligned()
                .left()
                .flex(1.0, true);
            let excluded_files_view = ChildView::new(&search.excluded_files_editor, cx)
                .aligned()
                .right()
                .flex(1.0, true);
            let regex_button = self.render_regex_button("Regex", search.current_mode.clone(), cx);
            let row_spacing = theme.workspace.toolbar.container.padding.bottom;
            let search = _search.read(cx);
            let filter_button = {
                let tooltip_style = theme::current(cx).tooltip.clone();
                let is_active = search.filters_enabled;
                MouseEventHandler::<Self, _>::new(0, cx, |state, cx| {
                    let theme = theme::current(cx);
                    let style = theme
                        .search
                        .option_button
                        .in_state(is_active)
                        .style_for(state);
                    Svg::new("icons/filter_12.svg")
                        .with_color(style.text.color.clone())
                        .contained()
                        .with_style(style.container)
                })
                .on_click(MouseButton::Left, move |_, this, cx| {
                    this.toggle_filters(cx);
                })
                .with_cursor_style(CursorStyle::PointingHand)
                .with_tooltip::<Self>(0, "Toggle filters".into(), None, tooltip_style, cx)
                .into_any()
            };
            let search = _search.read(cx);
            let is_semantic_disabled = search.semantic.is_none();

            let case_sensitive = if is_semantic_disabled {
                Some(self.render_option_button_icon(
                    "icons/case_insensitive_12.svg",
                    SearchOptions::CASE_SENSITIVE,
                    cx,
                ))
            } else {
                None
            };

            let whole_word = if is_semantic_disabled {
                Some(self.render_option_button_icon(
                    "icons/word_search_12.svg",
                    SearchOptions::WHOLE_WORD,
                    cx,
                ))
            } else {
                None
            };

            let search = _search.read(cx);
            let icon_style = theme.search.editor_icon.clone();
            let query = Flex::row()
                .with_child(
                    Svg::for_style(icon_style.icon)
                        .contained()
                        .with_style(icon_style.container)
                        .constrained(),
                )
                .with_child(
                    ChildView::new(&search.query_editor, cx)
                        .constrained()
                        .flex(1., true)
                        .into_any(),
                )
                .with_child(
                    Flex::row()
                        .with_child(filter_button)
                        .with_children(whole_word)
                        .with_children(case_sensitive)
                        .flex(1., true)
                        .contained(),
                )
                .align_children_center()
                .aligned()
                .left()
                .flex(1., true);
            let search = _search.read(cx);
            let matches = search.active_match_index.map(|match_ix| {
                Label::new(
                    format!(
                        "{}/{}",
                        match_ix + 1,
                        search.model.read(cx).match_ranges.len()
                    ),
                    theme.search.match_index.text.clone(),
                )
                .contained()
                .with_style(theme.search.match_index.container)
                .aligned()
                .left()
            });

            let filters = search.filters_enabled.then(|| {
                Flex::row()
                    .with_child(
                        Flex::row()
                            .with_child(included_files_view)
                            .contained()
                            .with_style(include_container_style)
                            .aligned()
                            .constrained()
                            .with_min_width(theme.search.include_exclude_editor.min_width)
                            .with_max_width(theme.search.include_exclude_editor.max_width)
                            .flex(1., false),
                    )
                    .with_child(
                        Flex::row()
                            .with_child(excluded_files_view)
                            .contained()
                            .with_style(exclude_container_style)
                            .aligned()
                            .constrained()
                            .with_min_width(theme.search.include_exclude_editor.min_width)
                            .with_max_width(theme.search.include_exclude_editor.max_width)
                            .flex(1., false),
                    )
            });

            let semantic_index =
                SemanticIndex::enabled(cx).then(|| self.render_semantic_search_button(cx));
            let normal_search = self.render_text_search_button(cx);
            Flex::row()
                .with_child(
                    Flex::column()
                        .with_child(Flex::row().with_children(matches).aligned().left())
                        .flex(1., true),
                )
                .with_child(
                    Flex::column()
                        .with_child(
                            Flex::row()
                                .with_child(
                                    Flex::row()
                                        .with_child(query)
                                        .contained()
                                        .with_style(query_container_style)
                                        .aligned()
                                        .constrained()
                                        .with_min_width(theme.search.editor.min_width)
                                        .with_max_width(theme.search.editor.max_width)
                                        .flex(1., false),
                                )
                                .with_child(
                                    Flex::row()
                                        .with_child(self.render_nav_button(
                                            "<",
                                            Direction::Prev,
                                            cx,
                                        ))
                                        .with_child(self.render_nav_button(
                                            ">",
                                            Direction::Next,
                                            cx,
                                        ))
                                        .aligned(),
                                )
                                .contained()
                                .with_margin_bottom(row_spacing),
                        )
                        .with_children(filters)
                        .contained()
                        .with_style(theme.search.container)
                        .aligned()
                        .top()
                        .flex(2., true),
                )
                .with_child(
                    Flex::column().with_child(
                        Flex::row()
                            .with_child(normal_search)
                            .with_children(semantic_index)
                            .with_child(regex_button)
                            .constrained()
                            .with_height(theme.workspace.toolbar.height)
                            .contained()
                            .aligned()
                            .right()
                            .flex(1., true),
                    ),
                )
                .contained()
                .flex_float()
                .into_any_named("project search")
        } else {
            Empty::new().into_any()
        }
    }
}

impl ToolbarItemView for ProjectSearchBar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut ViewContext<Self>,
    ) -> ToolbarItemLocation {
        cx.notify();
        self.subscription = None;
        self.active_project_search = None;
        if let Some(search) = active_pane_item.and_then(|i| i.downcast::<ProjectSearchView>()) {
            self.subscription = Some(cx.observe(&search, |_, _, cx| cx.notify()));
            self.active_project_search = Some(search);
            ToolbarItemLocation::PrimaryLeft {
                flex: Some((1., false)),
            }
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn row_count(&self, cx: &ViewContext<Self>) -> usize {
        self.active_project_search
            .as_ref()
            .map(|search| {
                let offset = search.read(cx).filters_enabled as usize;
                1 + offset
            })
            .unwrap_or_else(|| 1)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use editor::DisplayPoint;
    use gpui::{color::Color, executor::Deterministic, TestAppContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;
    use theme::ThemeSettings;

    #[gpui::test]
    async fn test_project_search(deterministic: Arc<Deterministic>, cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background());
        fs.insert_tree(
            "/dir",
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let search = cx.add_model(|cx| ProjectSearch::new(project, cx));
        let (_, search_view) = cx.add_window(|cx| ProjectSearchView::new(search.clone(), cx));

        search_view.update(cx, |search_view, cx| {
            search_view
                .query_editor
                .update(cx, |query_editor, cx| query_editor.set_text("TWO", cx));
            search_view.search(cx);
        });
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;"
            );
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.all_background_highlights(cx)),
                &[
                    (
                        DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35),
                        Color::red()
                    ),
                    (
                        DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40),
                        Color::red()
                    ),
                    (
                        DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9),
                        Color::red()
                    )
                ]
            );
            assert_eq!(search_view.active_match_index, Some(0));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35)]
            );

            search_view.select_match(Direction::Next, cx);
        });

        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.active_match_index, Some(1));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40)]
            );
            search_view.select_match(Direction::Next, cx);
        });

        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.active_match_index, Some(2));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9)]
            );
            search_view.select_match(Direction::Next, cx);
        });

        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.active_match_index, Some(0));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(2, 32)..DisplayPoint::new(2, 35)]
            );
            search_view.select_match(Direction::Prev, cx);
        });

        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.active_match_index, Some(2));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(5, 6)..DisplayPoint::new(5, 9)]
            );
            search_view.select_match(Direction::Prev, cx);
        });

        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.active_match_index, Some(1));
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.selections.display_ranges(cx)),
                [DisplayPoint::new(2, 37)..DisplayPoint::new(2, 40)]
            );
        });
    }

    #[gpui::test]
    async fn test_project_search_focus(deterministic: Arc<Deterministic>, cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.background());
        fs.insert_tree(
            "/dir",
            json!({
                "one.rs": "const ONE: usize = 1;",
                "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let (window_id, workspace) = cx.add_window(|cx| Workspace::test_new(project, cx));

        let active_item = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_item.is_none(),
            "Expected no search panel to be active, but got: {active_item:?}"
        );

        workspace.update(cx, |workspace, cx| {
            ProjectSearchView::deploy(workspace, &workspace::NewSearch, cx)
        });

        let Some(search_view) = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        }) else {
            panic!("Search view expected to appear after new search event trigger")
        };
        let search_view_id = search_view.id();

        cx.spawn(
            |mut cx| async move { cx.dispatch_action(window_id, search_view_id, &ToggleFocus) },
        )
        .detach();
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert!(
                search_view.query_editor.is_focused(cx),
                "Empty search view should be focused after the toggle focus event: no results panel to focus on",
            );
        });

        search_view.update(cx, |search_view, cx| {
            let query_editor = &search_view.query_editor;
            assert!(
                query_editor.is_focused(cx),
                "Search view should be focused after the new search view is activated",
            );
            let query_text = query_editor.read(cx).text(cx);
            assert!(
                query_text.is_empty(),
                "New search query should be empty but got '{query_text}'",
            );
            let results_text = search_view
                .results_editor
                .update(cx, |editor, cx| editor.display_text(cx));
            assert!(
                results_text.is_empty(),
                "Empty search view should have no results but got '{results_text}'"
            );
        });

        search_view.update(cx, |search_view, cx| {
            search_view.query_editor.update(cx, |query_editor, cx| {
                query_editor.set_text("sOMETHINGtHATsURELYdOESnOTeXIST", cx)
            });
            search_view.search(cx);
        });
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            let results_text = search_view
                .results_editor
                .update(cx, |editor, cx| editor.display_text(cx));
            assert!(
                results_text.is_empty(),
                "Search view for mismatching query should have no results but got '{results_text}'"
            );
            assert!(
                search_view.query_editor.is_focused(cx),
                "Search view should be focused after mismatching query had been used in search",
            );
        });
        cx.spawn(
            |mut cx| async move { cx.dispatch_action(window_id, search_view_id, &ToggleFocus) },
        )
        .detach();
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert!(
                search_view.query_editor.is_focused(cx),
                "Search view with mismatching query should be focused after the toggle focus event: still no results panel to focus on",
            );
        });

        search_view.update(cx, |search_view, cx| {
            search_view
                .query_editor
                .update(cx, |query_editor, cx| query_editor.set_text("TWO", cx));
            search_view.search(cx);
        });
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                "Search view results should match the query"
            );
            assert!(
                search_view.results_editor.is_focused(cx),
                "Search view with mismatching query should be focused after search results are available",
            );
        });
        cx.spawn(
            |mut cx| async move { cx.dispatch_action(window_id, search_view_id, &ToggleFocus) },
        )
        .detach();
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert!(
                search_view.results_editor.is_focused(cx),
                "Search view with matching query should still have its results editor focused after the toggle focus event",
            );
        });

        workspace.update(cx, |workspace, cx| {
            ProjectSearchView::deploy(workspace, &workspace::NewSearch, cx)
        });
        search_view.update(cx, |search_view, cx| {
            assert_eq!(search_view.query_editor.read(cx).text(cx), "two", "Query should be updated to first search result after search view 2nd open in a row");
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst THREE: usize = one::ONE + two::TWO;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                "Results should be unchanged after search view 2nd open in a row"
            );
            assert!(
                search_view.query_editor.is_focused(cx),
                "Focus should be moved into query editor again after search view 2nd open in a row"
            );
        });

        cx.spawn(
            |mut cx| async move { cx.dispatch_action(window_id, search_view_id, &ToggleFocus) },
        )
        .detach();
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert!(
                search_view.results_editor.is_focused(cx),
                "Search view with matching query should switch focus to the results editor after the toggle focus event",
            );
        });
    }

    #[gpui::test]
    async fn test_new_project_search_in_directory(
        deterministic: Arc<Deterministic>,
        cx: &mut TestAppContext,
    ) {
        init_test(cx);

        let fs = FakeFs::new(cx.background());
        fs.insert_tree(
            "/dir",
            json!({
                "a": {
                    "one.rs": "const ONE: usize = 1;",
                    "two.rs": "const TWO: usize = one::ONE + one::ONE;",
                },
                "b": {
                    "three.rs": "const THREE: usize = one::ONE + two::TWO;",
                    "four.rs": "const FOUR: usize = one::ONE + three::THREE;",
                },
            }),
        )
        .await;
        let project = Project::test(fs.clone(), ["/dir".as_ref()], cx).await;
        let worktree_id = project.read_with(cx, |project, cx| {
            project.worktrees(cx).next().unwrap().read(cx).id()
        });
        let (_, workspace) = cx.add_window(|cx| Workspace::test_new(project, cx));

        let active_item = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_item.is_none(),
            "Expected no search panel to be active, but got: {active_item:?}"
        );

        let one_file_entry = cx.update(|cx| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&(worktree_id, "a/one.rs").into(), cx)
                .expect("no entry for /a/one.rs file")
        });
        assert!(one_file_entry.is_file());
        workspace.update(cx, |workspace, cx| {
            ProjectSearchView::new_search_in_directory(workspace, &one_file_entry, cx)
        });
        let active_search_entry = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        });
        assert!(
            active_search_entry.is_none(),
            "Expected no search panel to be active for file entry"
        );

        let a_dir_entry = cx.update(|cx| {
            workspace
                .read(cx)
                .project()
                .read(cx)
                .entry_for_path(&(worktree_id, "a").into(), cx)
                .expect("no entry for /a/ directory")
        });
        assert!(a_dir_entry.is_dir());
        workspace.update(cx, |workspace, cx| {
            ProjectSearchView::new_search_in_directory(workspace, &a_dir_entry, cx)
        });

        let Some(search_view) = cx.read(|cx| {
            workspace
                .read(cx)
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<ProjectSearchView>())
        }) else {
            panic!("Search view expected to appear after new search in directory event trigger")
        };
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert!(
                search_view.query_editor.is_focused(cx),
                "On new search in directory, focus should be moved into query editor"
            );
            search_view.excluded_files_editor.update(cx, |editor, cx| {
                assert!(
                    editor.display_text(cx).is_empty(),
                    "New search in directory should not have any excluded files"
                );
            });
            search_view.included_files_editor.update(cx, |editor, cx| {
                assert_eq!(
                    editor.display_text(cx),
                    a_dir_entry.path.join("**").display().to_string(),
                    "New search in directory should have included dir entry path"
                );
            });
        });

        search_view.update(cx, |search_view, cx| {
            search_view
                .query_editor
                .update(cx, |query_editor, cx| query_editor.set_text("const", cx));
            search_view.search(cx);
        });
        deterministic.run_until_parked();
        search_view.update(cx, |search_view, cx| {
            assert_eq!(
                search_view
                    .results_editor
                    .update(cx, |editor, cx| editor.display_text(cx)),
                "\n\nconst ONE: usize = 1;\n\n\nconst TWO: usize = one::ONE + one::ONE;",
                "New search in directory should have a filter that matches a certain directory"
            );
        });
    }

    pub fn init_test(cx: &mut TestAppContext) {
        cx.foreground().forbid_parking();
        let fonts = cx.font_cache();
        let mut theme = gpui::fonts::with_font_cache(fonts.clone(), theme::Theme::default);
        theme.search.match_background = Color::red();

        cx.update(|cx| {
            cx.set_global(SettingsStore::test(cx));
            cx.set_global(ActiveSearches::default());

            theme::init((), cx);
            cx.update_global::<SettingsStore, _, _>(|store, _| {
                let mut settings = store.get::<ThemeSettings>(None).clone();
                settings.theme = Arc::new(theme);
                store.override_global(settings)
            });

            language::init(cx);
            client::init_settings(cx);
            editor::init(cx);
            workspace::init_settings(cx);
            Project::init_settings(cx);
            super::init(cx);
        });
    }
}
