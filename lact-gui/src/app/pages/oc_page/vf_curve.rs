use crate::{
    I18N,
    app::{APP_BROKER, graphs_window::plot::PlotColorScheme, msg::AppMsg},
};
use amdgpu_sysfs::gpu_handle::PowerLevelId;
use gtk::{gdk, glib, prelude::*};
use i18n_embed_fl::fl;
use indexmap::IndexMap;
use lact_schema::{ClocksTable, DeviceStats, NvidiaVfPoint, config};
use plotters::{
    chart::{ChartBuilder, SeriesLabelPosition},
    prelude::{Circle, EmptyElement, IntoDrawingArea as _, Rectangle, Text},
    series::{AreaSeries, DashedLineSeries, LineSeries, PointSeries},
    style::{Color as _, ShapeStyle, TextStyle, text_anchor::Pos},
};
use plotters_cairo::CairoBackend;
use relm4::{ComponentParts, RelmObjectExt as _, RelmWidgetExt as _, binding::BoolBinding, css};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeSet,
    rc::Rc,
    sync::Arc,
};
use std::{cmp, fmt::Write as _};

// In percentage
const POINT_VOLTAGE_HOVER_MARGIN: f32 = 0.01;
const POINT_FREQ_HOVER_MARGIN: f32 = 0.03;

// In clock steps of the card, see `freq_step`
const KEY_STEPS: i32 = 1;
const KEY_STEPS_COARSE: i32 = 5;

// In MHz, a larger detected step is treated as a coincidence
const MAX_FREQ_STEP: u32 = 100;

// In points
const MIN_VISIBLE_POINTS: usize = 4;

// In MHz
const VISIBLE_FREQ_RANGE_PADDING_MIN: u32 = 100;
const VISIBLE_FREQ_RANGE_PADDING_MAX: u32 = 300;

#[derive(Clone)]
pub struct VfCurveEditor {
    /// `freq` is drawn and dragged, `freq_offset` is what gets exported - see [`set_point_freq`]
    points: Rc<RefCell<Vec<NvidiaVfPoint>>>,
    stats: Rc<RefCell<Arc<DeviceStats>>>,
    allow_editing: BoolBinding,
    locked_clocks_range: Rc<Cell<Option<(u32, u32)>>>,
    freq_range: Rc<Cell<Option<(u32, u32)>>>,
    /// The driver rounds frequencies to a grid (15 MHz on most cards), edits are snapped to it
    /// so that the curve does not change shape when it is applied
    freq_step: Rc<Cell<u32>>,

    /// Offset in MHz that the "set offset" button puts on the whole base curve
    curve_offset: gtk::Adjustment,

    visible_range_start: gtk::Adjustment,
    visible_range_end: gtk::Adjustment,

    // Passed from app
    global_settings_changed: BoolBinding,

    cursor_position: Rc<Cell<Option<(f64, f64)>>>,
    hovered_coords: Rc<Cell<Option<(u32, u32)>>>,
    hovered_point: Rc<Cell<Option<usize>>>,
    dragging_point: Rc<Cell<Option<usize>>>,
    drag_modifiers: Rc<Cell<gdk::ModifierType>>,
    // This is stored to avoid updating the range mid-drag
    cached_y_spec: Rc<Cell<Option<(u32, u32)>>>,

    /// Indices into `points` (not into the visible slice)
    selected_points: Rc<RefCell<BTreeSet<usize>>>,
    /// Fixed end of a keyboard range selection
    selection_anchor: Rc<Cell<Option<usize>>>,
    /// Moving end of a keyboard range selection, also the point that shows its label
    selection_cursor: Rc<Cell<Option<usize>>>,
    /// Voltage where a rubber band selection was started
    rubber_band_start: Rc<Cell<Option<usize>>>,
    /// Distance between the cursor and the grabbed point, so that a click does not move it
    drag_grab_offset: Rc<Cell<i32>>,
}

#[derive(Debug, Clone, Copy)]
pub enum VfCurveKeyAction {
    /// Shift the selected points by a number of clock steps
    Nudge(i32),
    /// Move the selection to a neighbouring point, optionally extending it
    MoveCursor { delta: isize, extend: bool },
    SelectAll,
    ClearSelection,
    ZoomIn,
    ZoomOut,
    ResetZoom,
    LineBetweenSelected,
}

pub struct VfCurveEditorInit {
    pub global_settings_changed: BoolBinding,
    pub allow_editing: BoolBinding,
}

#[derive(Debug)]
pub enum VfCurveEditorMsg {
    Show,
    Clocks(Option<Arc<ClocksTable>>),
    Stats(Arc<DeviceStats>),
    CursorUpdate {
        x: f64,
        y: f64,
        modifiers: gdk::ModifierType,
    },
    DragStart {
        modifiers: gdk::ModifierType,
    },
    DragEnd,
    Key(VfCurveKeyAction),
    /// Mouse wheel zoom around the cursor position
    ScrollZoom {
        zoom_in: bool,
    },
    /// Shift the visible range by a number of points
    Pan(isize),
    FlattenCurve,
    FlattenSelection,
    /// Put the points between the outermost selected ones on a straight line
    LineBetweenSelected,
    /// Set every point to its base frequency plus the configured offset
    ApplyCurveOffset,
    ResetCurve,
}

#[relm4::component(pub)]
impl relm4::Component for VfCurveEditor {
    type Init = VfCurveEditorInit;
    type Input = VfCurveEditorMsg;
    type Output = ();
    type CommandOutput = ();

    view! {
        #[root]
        adw::Window {
            set_hide_on_close: true,
            set_default_size: (1100, 700),
            set_title: Some(&fl!(I18N, "vf-curve-editor")),

            adw::ToolbarView {
                add_top_bar = &adw::HeaderBar {},

                #[wrap(Some)]
                set_content = &gtk::Box {
                    set_orientation: gtk::Orientation::Vertical,
                    set_margin_all: 15,
                    set_spacing: 10,

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_halign: gtk::Align::Center,
                        set_spacing: 10,

                        gtk::Label {
                            set_markup: &fl!(I18N, "nvidia-vf-curve-warning"),
                            add_css_class: css::WARNING,
                            add_css_class: css::HEADING,
                        },

                    },


                    #[name = "drawing_area"]
                    gtk::DrawingArea {
                        set_expand: true,
                        set_focusable: true,

                        set_draw_func[model] => move |_, ctx, width, height| {
                            model.draw_chart(ctx, width, height, PlotColorScheme::current());
                        },

                        add_controller = gtk::GestureClick {
                            connect_pressed[sender, drawing_area] => move |gesture, _, x, y| {
                                let modifiers = gesture.current_event_state();
                                drawing_area.grab_focus();

                                sender.input(VfCurveEditorMsg::DragStart { modifiers });
                                sender.input(VfCurveEditorMsg::CursorUpdate { x, y, modifiers });
                            },
                            connect_released[sender] => move |_, _, _x, _y| {
                                sender.input(VfCurveEditorMsg::DragEnd);
                            }
                        },

                        add_controller = gtk::GestureClick {
                            set_button: gdk::BUTTON_SECONDARY,
                            connect_pressed[drawing_area, point_menu, model] => move |_, _, x, y| {
                                if model.hovered_point.get().is_none() || !model.allow_editing.value() {
                                    return;
                                }

                                point_menu.set_parent(&drawing_area);
                                point_menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                                point_menu.popup();

                            },
                        },

                        add_controller = gtk::EventControllerKey {
                            connect_key_pressed[sender] => move |_, key, _, modifiers| {
                                match key_action(key, modifiers) {
                                    Some(action) => {
                                        sender.input(VfCurveEditorMsg::Key(action));
                                        glib::Propagation::Stop
                                    }
                                    None => glib::Propagation::Proceed,
                                }
                            },
                        },

                        add_controller = gtk::EventControllerScroll {
                            set_flags: gtk::EventControllerScrollFlags::VERTICAL
                                | gtk::EventControllerScrollFlags::DISCRETE,
                            connect_scroll[sender] => move |scroll, _, dy| {
                                if dy != 0.0 {
                                    let shift = scroll
                                        .current_event_state()
                                        .contains(gdk::ModifierType::SHIFT_MASK);
                                    if shift {
                                        sender.input(VfCurveEditorMsg::Pan(if dy > 0.0 { 1 } else { -1 }));
                                    } else {
                                        sender.input(VfCurveEditorMsg::ScrollZoom { zoom_in: dy < 0.0 });
                                    }
                                }
                                glib::Propagation::Stop
                            },
                        },

                        add_controller = gtk::EventControllerMotion {
                            connect_motion[sender] => move |motion, x, y| {
                                let modifiers = motion.current_event_state();
                                sender.input(VfCurveEditorMsg::CursorUpdate { x, y, modifiers });
                            },
                        },

                        #[watch]
                        set_cursor: if model.dragging_point.get().is_some() {
                            gdk::Cursor::from_name("move", None)
                        } else if model.rubber_band_start.get().is_some() {
                            gdk::Cursor::from_name("col-resize", None)
                        } else {
                            None
                        }.as_ref(),
                    },

                    gtk::Label {
                        set_label: &fl!(I18N, "vf-curve-shortcuts"),
                        set_wrap: true,
                        set_justify: gtk::Justification::Center,
                        set_halign: gtk::Align::Center,
                        add_css_class: css::DIM_LABEL,
                        add_css_class: css::CAPTION,
                        add_write_only_binding: (&model.allow_editing, "visible"),
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 5,
                        add_write_only_binding: (&model.allow_editing, "visible"),

                        gtk::Label {
                            set_label: &fl!(I18N, "vf-curve-base-offset"),
                            add_css_class: css::HEADING,
                        },

                        gtk::SpinButton {
                            set_adjustment: &model.curve_offset,
                            set_digits: 0,
                            set_numeric: true,
                            set_width_chars: 6,
                            set_valign: gtk::Align::Center,
                        },

                        gtk::Button {
                            set_label: &fl!(I18N, "vf-curve-apply-offset"),
                            set_valign: gtk::Align::Center,
                            connect_clicked => VfCurveEditorMsg::ApplyCurveOffset,
                        },

                        gtk::Button {
                            set_label: &fl!(I18N, "vf-curve-line-between"),
                            set_valign: gtk::Align::Center,
                            set_halign: gtk::Align::End,
                            set_hexpand: true,

                            #[watch]
                            set_sensitive: model.line_endpoints().is_some(),

                            connect_clicked => VfCurveEditorMsg::LineBetweenSelected,
                        },
                    },

                    gtk::Box {
                        set_orientation: gtk::Orientation::Horizontal,
                        set_spacing: 5,

                        gtk::Label {
                            set_label: &fl!(I18N, "vf-curve-visible-range"),
                            add_css_class: css::HEADING,
                        },

                        gtk::Scale {
                            set_adjustment: &model.visible_range_start,
                            set_draw_value: true,
                            set_width_request: 150,
                            set_value_pos: gtk::PositionType::Right,
                            set_digits: 0,
                        },


                        gtk::Label {
                            set_label: &fl!(I18N, "vf-curve-visible-range-to"),
                            add_css_class: css::HEADING,
                        },

                        gtk::Scale {
                            set_adjustment: &model.visible_range_end,
                            set_draw_value: true,
                            set_width_request: 150,
                            set_value_pos: gtk::PositionType::Right,
                            set_digits: 0,
                        },

                        #[name = "editing_disabled_label"]
                        gtk::Label {
                            set_label: &fl!(I18N, "vf-curve-editing-disabled"),
                            set_halign: gtk::Align::Center,
                            set_hexpand: true,
                            set_valign: gtk::Align::Center,
                            add_css_class: css::DIM_LABEL,
                        },

                        gtk::Box {
                            set_orientation: gtk::Orientation::Horizontal,
                            set_spacing: 5,
                            set_halign: gtk::Align::End,
                            set_hexpand: true,
                            add_write_only_binding: (&model.allow_editing, "visible"),

                            gtk::Button {
                                set_label: &fl!(I18N, "default-button"),
                                add_css_class: css::DESTRUCTIVE_ACTION,
                                set_valign: gtk::Align::Center,

                                #[watch]
                                set_sensitive: curve_has_offsets(&model.points.borrow()),

                                connect_clicked => VfCurveEditorMsg::ResetCurve,
                                connect_clicked => move |_| {
                                    APP_BROKER.send(AppMsg::SettingsChanged);
                                }
                            },

                            gtk::Button {
                                set_label: &fl!(I18N, "apply-button"),
                                add_binding: (&model.global_settings_changed, "sensitive"),
                                add_css_class: css::SUGGESTED_ACTION,
                                set_valign: gtk::Align::Center,

                                connect_clicked => move |_| {
                                    APP_BROKER.send(AppMsg::ApplyChanges);
                                },
                            },
                        },
                    },
                },
            },
        },

        #[name = "point_menu"]
        gtk::Popover {
            add_css_class: css::MENU,

            connect_closed => |popover| {
                popover.unparent();
            },

            gtk::Box {
                add_css_class: css::FLAT,
                set_orientation: gtk::Orientation::Vertical,

                gtk::Button {
                    set_label: &fl!(I18N, "vf-curve-flatten-selection"),
                    connect_clicked => VfCurveEditorMsg::FlattenSelection,
                    connect_clicked[point_menu] => move |_| {
                        point_menu.popdown();
                    },
                    add_css_class: css::FLAT,
                    #[watch]
                    set_visible: model.selected_points.borrow().len() > 1,
                },

                gtk::Button {
                    set_label: &fl!(I18N, "vf-curve-line-between"),
                    connect_clicked => VfCurveEditorMsg::LineBetweenSelected,
                    connect_clicked[point_menu] => move |_| {
                        point_menu.popdown();
                    },
                    add_css_class: css::FLAT,
                    #[watch]
                    set_visible: model.line_endpoints().is_some(),
                },

                gtk::Button {
                    set_label: &fl!(I18N, "vf-curve-flatten-right"),
                    connect_clicked => VfCurveEditorMsg::FlattenCurve,
                    connect_clicked[point_menu] => move |_| {
                        point_menu.popdown();
                    },
                    add_css_class: css::FLAT,
                },
            },
        }
    }

    fn init(
        VfCurveEditorInit {
            global_settings_changed,
            allow_editing,
        }: Self::Init,
        root: Self::Root,
        sender: relm4::ComponentSender<Self>,
    ) -> relm4::ComponentParts<Self> {
        let model = Self {
            points: Rc::default(),
            stats: Rc::default(),
            global_settings_changed,
            locked_clocks_range: Rc::default(),
            freq_range: Rc::default(),
            freq_step: Rc::new(Cell::new(1)),
            allow_editing,
            cursor_position: Rc::new(Cell::new(None)),
            curve_offset: gtk::Adjustment::new(0.0, -1000.0, 1000.0, 5.0, 25.0, 0.0),
            visible_range_start: gtk::Adjustment::new(30.0, 0.0, 100.0, 1.0, 10.0, 0.0),
            visible_range_end: gtk::Adjustment::new(100.0, 0.0, 100.0, 1.0, 10.0, 0.0),
            hovered_coords: Rc::new(Cell::new(None)),
            hovered_point: Rc::new(Cell::new(None)),
            dragging_point: Rc::new(Cell::new(None)),
            drag_modifiers: Rc::new(Cell::new(gdk::ModifierType::empty())),
            cached_y_spec: Rc::new(Cell::new(None)),
            selected_points: Rc::default(),
            selection_anchor: Rc::new(Cell::new(None)),
            selection_cursor: Rc::new(Cell::new(None)),
            rubber_band_start: Rc::new(Cell::new(None)),
            drag_grab_offset: Rc::new(Cell::new(0)),
        };

        let widgets = view_output!();

        let drawing_area = widgets.drawing_area.clone();
        model
            .allow_editing
            .connect_value_notify(move |_| drawing_area.queue_draw());

        model
            .allow_editing
            .bind_property("value", &widgets.editing_disabled_label, "visible")
            .invert_boolean()
            .sync_create()
            .build();

        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        widgets: &mut Self::Widgets,
        msg: Self::Input,
        sender: relm4::ComponentSender<Self>,
        root: &Self::Root,
    ) {
        match msg {
            VfCurveEditorMsg::Show => {
                root.present();
                widgets.drawing_area.grab_focus();
            }
            VfCurveEditorMsg::Clocks(clocks_table) => {
                let mut points = self.points.borrow_mut();
                points.clear();
                self.locked_clocks_range.take();
                self.freq_range.take();
                self.freq_step.set(1);

                if let Some(ClocksTable::Nvidia(nvidia_table)) = clocks_table.as_deref() {
                    points.extend_from_slice(&nvidia_table.gpu_vf_curve);
                    self.locked_clocks_range.set(nvidia_table.gpu_locked_clocks);
                    let offset_range = nvidia_table
                        .gpu_offsets
                        .get(&0)
                        .map(|offset| (offset.min, offset.max));
                    self.freq_range
                        .set(Self::freq_limits_range(&points, offset_range));

                    let freq_step = detect_freq_step(&points);
                    self.freq_step.set(freq_step);
                    self.curve_offset.set_step_increment(freq_step as f64);
                    self.curve_offset.set_page_increment((freq_step * 5) as f64);

                    if let Some((min_offset, max_offset)) = offset_range
                        && min_offset < max_offset
                    {
                        self.curve_offset.set_lower(min_offset as f64);
                        self.curve_offset.set_upper(max_offset as f64);
                    }
                }

                let len = points.len();
                self.selected_points.borrow_mut().retain(|i| *i < len);

                if points.is_empty() {
                    root.set_visible(false);
                }
            }
            VfCurveEditorMsg::Stats(device_stats) => {
                *self.stats.borrow_mut() = device_stats;
            }
            VfCurveEditorMsg::CursorUpdate { x, y, modifiers } => {
                self.cursor_position.set(Some((x, y)));
                self.drag_modifiers.set(modifiers);
            }
            VfCurveEditorMsg::DragStart { modifiers } => {
                if !self.allow_editing.value() {
                    return;
                }

                self.cached_y_spec.take();
                let (visible_start, _) = self.visible_points_range();

                if let Some(point) = self.hovered_point.get() {
                    let idx = visible_start + point;
                    let mut selection = self.selected_points.borrow_mut();
                    let mut start_drag = true;

                    if modifiers.contains(gdk::ModifierType::CONTROL_MASK) {
                        // Ctrl+click toggles a single point
                        if !selection.insert(idx) {
                            selection.remove(&idx);
                            start_drag = false;
                        }
                    } else if !selection.contains(&idx) {
                        selection.clear();
                        selection.insert(idx);
                    }

                    self.selection_anchor.set(Some(idx));
                    self.selection_cursor.set(Some(idx));

                    if start_drag {
                        let grab_offset = match (
                            self.hovered_coords.get(),
                            self.points.borrow().get(idx),
                        ) {
                            (Some((_, freq)), Some(point)) => freq as i32 - point.freq as i32,
                            _ => 0,
                        };
                        self.drag_grab_offset.set(grab_offset);
                        self.dragging_point.set(Some(point));
                    }
                } else {
                    self.rubber_band_start
                        .set(self.hovered_coords.get().map(|(x, _)| x as usize));
                }
            }
            VfCurveEditorMsg::DragEnd => {
                let was_dragging_point = self.dragging_point.take().is_some();
                self.cached_y_spec.take();

                if was_dragging_point {
                    APP_BROKER.send(AppMsg::SettingsChanged);
                } else if let Some((band_start, band_end)) = self.get_band_voltage_range() {
                    let additive = self
                        .drag_modifiers
                        .get()
                        .contains(gdk::ModifierType::CONTROL_MASK);

                    let (visible_start, visible_end) = self.visible_points_range();
                    let points = self.points.borrow();
                    let banded = (visible_start..visible_end)
                        .filter(|i| {
                            let voltage = points[*i].voltage as usize;
                            band_start < voltage && voltage < band_end
                        })
                        .collect::<Vec<_>>();

                    let mut selection = self.selected_points.borrow_mut();
                    if !additive {
                        selection.clear();
                        self.selection_anchor.set(banded.first().copied());
                        self.selection_cursor.set(banded.last().copied());
                    } else if !banded.is_empty() {
                        self.selection_anchor.set(banded.first().copied());
                        self.selection_cursor.set(banded.last().copied());
                    }
                    selection.extend(banded);
                }
                self.rubber_band_start.take();
            }
            VfCurveEditorMsg::Key(action) => {
                if self.dragging_point.get().is_some() {
                    return;
                }
                match action {
                    VfCurveKeyAction::ZoomIn => self.zoom(true, self.keyboard_zoom_focus()),
                    VfCurveKeyAction::ZoomOut => self.zoom(false, self.keyboard_zoom_focus()),
                    VfCurveKeyAction::LineBetweenSelected => {
                        sender.input(VfCurveEditorMsg::LineBetweenSelected);
                    }
                    VfCurveKeyAction::ResetZoom => {
                        let len = self.points.borrow().len();
                        self.set_visible_points_range(0, len);
                    }
                    _ if self.allow_editing.value() => self.handle_key_action(action),
                    _ => (),
                }
            }
            VfCurveEditorMsg::ScrollZoom { zoom_in } => {
                if self.dragging_point.get().is_some() {
                    return;
                }
                self.zoom(zoom_in, self.pointer_zoom_focus());
            }
            VfCurveEditorMsg::Pan(direction) => {
                if self.dragging_point.get().is_some() {
                    return;
                }
                let (start, end) = self.visible_points_range();
                let step = cmp::max((end - start) / 10, 1) as isize;
                self.pan(direction * step);
            }
            VfCurveEditorMsg::FlattenCurve => {
                if let Some(base_point_idx) = self.hovered_point.get() {
                    let (start, end) = self.visible_points_range();
                    let mut points = self.points.borrow_mut();
                    let points = &mut points[start..end];

                    let target_freq = points[base_point_idx].freq;

                    for point in points.iter_mut().skip(base_point_idx) {
                        set_point_freq(point, target_freq);
                    }

                    APP_BROKER.send(AppMsg::SettingsChanged);
                }
            }

            VfCurveEditorMsg::FlattenSelection => {
                if let Some(base_point_idx) = self.hovered_point.get() {
                    let (start, end) = self.visible_points_range();
                    let mut points = self.points.borrow_mut();

                    let target_freq = points[start + base_point_idx].freq;

                    for idx in self.selected_points.borrow().iter() {
                        if (start..end).contains(idx) {
                            set_point_freq(&mut points[*idx], target_freq);
                        }
                    }

                    APP_BROKER.send(AppMsg::SettingsChanged);
                }
            }
            VfCurveEditorMsg::LineBetweenSelected => {
                if !self.allow_editing.value() {
                    return;
                }

                if let Some((first, last)) = self.line_endpoints()
                    && let Some(freq_range) = self.freq_range.get()
                {
                    let freq_step = self.freq_step.get();
                    let mut points = self.points.borrow_mut();
                    let (start_voltage, start_freq) = vf_point_coords(&points[first]);
                    let (end_voltage, end_freq) = vf_point_coords(&points[last]);

                    if start_voltage != end_voltage {
                        let voltage_span = end_voltage as f64 - start_voltage as f64;
                        let freq_span = end_freq as f64 - start_freq as f64;

                        for point in &mut points[first + 1..last] {
                            let ratio = (point.voltage as f64 - start_voltage as f64) / voltage_span;
                            let freq = (start_freq as f64 + ratio * freq_span).round() as u32;
                            set_point_freq(point, snap_freq(freq, freq_step, freq_range));
                        }

                        APP_BROKER.send(AppMsg::SettingsChanged);
                    }
                }
            }
            VfCurveEditorMsg::ApplyCurveOffset => {
                if !self.allow_editing.value() {
                    return;
                }

                if let Some(freq_range) = self.freq_range.get() {
                    let offset = self.curve_offset.value().round() as i32;
                    let freq_step = self.freq_step.get();
                    for point in self.points.borrow_mut().iter_mut() {
                        let freq = offset_freq(point.base_freq, offset);
                        set_point_freq(point, snap_freq(freq, freq_step, freq_range));
                    }

                    APP_BROKER.send(AppMsg::SettingsChanged);
                }
            }
            VfCurveEditorMsg::ResetCurve => {
                let mut points = self.points.borrow_mut();
                for point in points.iter_mut() {
                    point.freq = point.base_freq;
                    point.freq_offset = 0;
                }
            }
        }

        self.update_view(widgets, sender);
    }

    fn post_view() {
        drawing_area.queue_draw();
    }
}

impl VfCurveEditor {
    fn draw_chart(&self, ctx: &cairo::Context, width: i32, height: i32, colors: PlotColorScheme) {
        let (visible_start, visible_end) = self.visible_points_range();

        let mut points = self.points.borrow_mut();

        let points = &mut points[visible_start..visible_end];

        if points.is_empty() {
            return;
        }

        let backend = CairoBackend::new(ctx, (width as u32, height as u32)).unwrap();

        let root = backend.into_drawing_area();
        root.fill(&colors.background).unwrap();

        let min_point = points.first().unwrap();
        let max_point = points.last().unwrap();

        let x_spec = min_point.base_voltage..max_point.base_voltage;
        let Some(freq_range) = self.freq_range.get() else {
            return;
        };

        let y_range = if self.dragging_point.get().is_some() {
            self.cached_y_spec.get().or_else(|| {
                let y_range = Self::visible_freq_range(points, freq_range);
                self.cached_y_spec.set(y_range);
                y_range
            })
        } else {
            Self::visible_freq_range(points, freq_range)
        };
        let Some((y_start, y_end)) = y_range else {
            return;
        };
        let y_spec = y_start..y_end;

        let mut chart = ChartBuilder::on(&root)
            .x_label_area_size(45)
            .y_label_area_size(110)
            .margin(50)
            .margin_bottom(20)
            .build_cartesian_2d(x_spec.clone(), y_spec.clone())
            .unwrap();

        chart
            .configure_mesh()
            .axis_style(colors.border_secondary)
            .bold_line_style(colors.border)
            .x_label_formatter(&|voltage| format!("{voltage} mV"))
            .y_label_formatter(&|clock| format!("{clock} MHz"))
            .x_label_style(("sans-serif", 14, &colors.text))
            .y_label_style(("sans-serif", 14, &colors.text))
            .x_desc(fl!(I18N, "voltage"))
            .y_desc(fl!(I18N, "frequency"))
            .draw()
            .unwrap();

        chart
            .draw_series(LineSeries::new(
                points.iter().map(vf_point_coords),
                &colors.success,
            ))
            .unwrap()
            .label(fl!(I18N, "vf-active-curve"))
            .legend(move |(x, y)| {
                Rectangle::new([(x - 15, y + 2), (x, y - 1)], colors.success.filled())
            });

        if curve_has_offsets(points) {
            let base_line_style = colors.success.mix(0.3);
            chart
                .draw_series(LineSeries::new(
                    points.iter().map(vf_point_base_coords),
                    &base_line_style,
                ))
                .unwrap()
                .label(fl!(I18N, "vf-base-curve"))
                .legend(move |(x, y)| {
                    Rectangle::new([(x - 15, y + 2), (x, y - 1)], base_line_style.filled())
                });
        }

        if let Some((min_freq, max_freq)) = self.locked_clocks_range.get() {
            let mut curves = vec![(
                [(x_spec.start, max_freq), (x_spec.end, max_freq)],
                fl!(I18N, "max-gpu-clock"),
                colors.error,
            )];

            if min_freq >= y_spec.start {
                curves.push((
                    [(x_spec.start, min_freq), (x_spec.end, min_freq)],
                    fl!(I18N, "min-gpu-clock"),
                    colors.warning,
                ));
            }

            for (curve, label, color) in curves {
                chart
                    .draw_series(LineSeries::new(curve, &color))
                    .unwrap()
                    .label(label)
                    .legend(move |(x, y)| {
                        Rectangle::new([(x - 15, y + 2), (x, y - 1)], color.filled())
                    });
            }
        }

        let mut main_label = None;

        let stats = self.stats.borrow();
        if let Some(current_voltage) = stats.voltage.gpu
            && let Some(current_clock) = stats.clockspeed.gpu_clockspeed
        {
            let mut label = format!("Current: {current_clock} MHz @ {current_voltage} mV");

            if stats
                .active_power_states
                .and_then(|states| states.core)
                .is_some_and(|state| state != PowerLevelId::Index(0))
            {
                label.push_str(" (Idle)");
            }

            main_label = Some(label);

            for line in [
                [
                    (current_voltage as u32, y_spec.start),
                    (current_voltage as u32, y_spec.end),
                ],
                [
                    (x_spec.start, current_clock as u32),
                    (x_spec.end, current_clock as u32),
                ],
            ] {
                chart
                    .draw_series(DashedLineSeries::new(
                        line,
                        8,
                        5,
                        ShapeStyle {
                            color: colors.text.mix(0.5),
                            filled: false,
                            stroke_width: 1,
                        },
                    ))
                    .unwrap();
            }
        }

        let active_style = colors.text;
        let hovered_style = colors.accent_bg;
        let selected_style = colors.accent_bg.mix(0.8);
        let selected_area_style = colors.text.mix(0.1);

        let hovered_point = self
            .dragging_point
            .get()
            .or_else(|| self.hovered_point.get());

        if !self.allow_editing.value() {
            self.selected_points.borrow_mut().clear();
            self.rubber_band_start.set(None);
        }

        let selected_points = self.selected_points.borrow();
        let band_voltage_range = self.get_band_voltage_range();
        let band_additive = self
            .drag_modifiers
            .get()
            .contains(gdk::ModifierType::CONTROL_MASK);
        let is_selected = |i: usize, voltage: u32| {
            let in_band = band_voltage_range.is_some_and(|(band_start, band_end)| {
                band_start < voltage as usize && (voltage as usize) < band_end
            });
            let in_selection = (band_voltage_range.is_none() || band_additive)
                && selected_points.contains(&(visible_start + i));
            in_band || in_selection
        };

        // Without a hovered point, show the label of the point that the keyboard is on
        let label_point = hovered_point.or_else(|| {
            self.selection_cursor
                .get()
                .filter(|cursor| selected_points.contains(cursor))
                .and_then(|cursor| cursor.checked_sub(visible_start))
                .filter(|cursor| *cursor < points.len())
        });

        let active_voltage = self.stats.borrow().voltage.gpu;

        if let Some((selected_start, selected_end)) = band_voltage_range {
            let x_values = [selected_start, selected_end];
            chart
                .draw_series(AreaSeries::new(
                    x_values.map(|x| (x as u32, y_spec.end)),
                    0,
                    selected_area_style,
                ))
                .unwrap();
        }

        let main_series = chart
            .draw_series(PointSeries::of_element(
                points.iter().map(vf_point_coords).enumerate(),
                3,
                ShapeStyle::from(&colors.success).filled(),
                &|(i, coord), mut size, mut style| {
                    if is_selected(i, coord.0) {
                        style.color = selected_style.to_rgba();
                        size = size * 5 / 3;
                    }

                    let is_active = active_voltage == Some(coord.0 as u64);
                    if is_active {
                        style.color = active_style.to_rgba();
                        size = size * 3 / 2;
                    }

                    let mut negative_offset = false;
                    let mut text_width = 160;

                    if hovered_point == Some(i) {
                        style.color = hovered_style.to_rgba();
                        size = cmp::max(size, 6);
                    }

                    let text = if label_point == Some(i) {
                        let point = points[i];

                        let mut text = format!("{} MHz", point.freq);

                        let offset = point.freq as i32 - point.base_freq as i32;
                        if offset != 0 {
                            let symbol = if offset > 0 {
                                "+"
                            } else {
                                negative_offset = true;
                                ""
                            };
                            write!(text, " ({symbol}{offset}) MHz").unwrap();
                            text_width += 50;
                        }

                        write!(text, " @ {} mV", point.voltage).unwrap();
                        text
                    } else {
                        String::new()
                    };

                    let text_style = TextStyle {
                        font: ("sans-serif", 15).into(),
                        color: colors.text.to_backend_color(),
                        pos: Pos::default(),
                    };

                    EmptyElement::at(coord)
                        + Circle::new((0, 0), size, style)
                        + Text::new(
                            text,
                            if negative_offset {
                                (0, 15)
                            } else {
                                (-text_width, -15)
                            },
                            text_style,
                        )
                },
            ))
            .unwrap();

        if let Some(label) = main_label {
            main_series
                .label(label)
                .legend(move |(x, y)| Circle::new((x - 7, y), 5, active_style.filled()));
        }

        chart
            .configure_series_labels()
            .position(SeriesLabelPosition::UpperLeft)
            .margin(20)
            .legend_area_size(5)
            .label_font(("sans-serif", 16, &colors.text))
            .position(SeriesLabelPosition::UpperLeft)
            .background_style(colors.background.mix(0.6))
            .draw()
            .unwrap();

        let translate = chart.into_coord_trans();

        let voltage_hover_margin =
            ((max_point.voltage - min_point.voltage) as f32 * POINT_VOLTAGE_HOVER_MARGIN) as i32;
        let freq_hover_margin =
            ((y_spec.end - y_spec.start) as f32 * POINT_FREQ_HOVER_MARGIN) as i32;
        let freq_hover_margin = freq_hover_margin.max(1);

        let hovered_coords = self
            .cursor_position
            .get()
            .and_then(|(x, y)| translate((x as i32, y as i32)));
        self.hovered_coords.set(hovered_coords);

        let hovered_point = hovered_coords.and_then(|(voltage, freq)| {
            points
                .iter()
                .enumerate()
                .map(|(i, point)| {
                    let voltage_distance = (voltage as i32 - point.voltage as i32).abs();
                    let freq_distance = (freq as i32 - point.freq as i32).abs();
                    (i, voltage_distance, freq_distance)
                })
                .filter(|(_, voltage_distance, freq_distance)| {
                    *voltage_distance < voltage_hover_margin && *freq_distance < freq_hover_margin
                })
                .min_by(|lhs, rhs| lhs.1.cmp(&rhs.1).then_with(|| lhs.2.cmp(&rhs.2)))
                .map(|(i, _, _)| i)
        });
        self.hovered_point.set(hovered_point);

        drop(selected_points);

        if let Some((_voltage, freq)) = hovered_coords
            && let Some(point_idx) = self.dragging_point.get()
        {
            let freq_step = self.freq_step.get();
            let new_freq = (freq as i32 - self.drag_grab_offset.get()).max(0) as u32;
            let new_freq = snap_freq(new_freq, freq_step, freq_range);
            let drag_delta = new_freq as i32 - points[point_idx].freq as i32;

            if self
                .drag_modifiers
                .get()
                .contains(gdk::ModifierType::SHIFT_MASK)
            {
                for point in points.iter_mut() {
                    shift_point_freq(point, drag_delta, freq_step, freq_range);
                }
            } else {
                for idx in self.selected_points.borrow().iter() {
                    if let Some(point) = idx
                        .checked_sub(visible_start)
                        .and_then(|i| points.get_mut(i))
                    {
                        shift_point_freq(point, drag_delta, freq_step, freq_range);
                    }
                }
            }
        }

        root.present().unwrap();
    }

    fn visible_points_range(&self) -> (usize, usize) {
        let len = self.points.borrow().len();
        let start = (len as f64 * (self.visible_range_start.value() / 100.0)) as usize;
        let start = cmp::min(start, len);

        let end = (len as f64 * (self.visible_range_end.value() / 100.0)) as usize;
        let end = cmp::min(end, len);

        (cmp::min(start, end), cmp::max(end, start))
    }

    /// Voltage range of the rubber band selection that is currently being dragged
    fn get_band_voltage_range(&self) -> Option<(usize, usize)> {
        let band_start = self.rubber_band_start.get()?;
        let band_end = self
            .hovered_coords
            .get()
            .map(|(x, _)| x as usize)
            .unwrap_or(band_start);

        Some((
            cmp::min(band_start, band_end),
            cmp::max(band_start, band_end),
        ))
    }

    fn handle_key_action(&self, action: VfCurveKeyAction) {
        let (visible_start, visible_end) = self.visible_points_range();
        if visible_start == visible_end {
            return;
        }
        let visible = visible_start..visible_end;
        let hovered = self.hovered_point.get().map(|i| visible_start + i);

        let mut selection = self.selected_points.borrow_mut();

        match action {
            VfCurveKeyAction::Nudge(steps) => {
                let Some(freq_range) = self.freq_range.get() else {
                    return;
                };
                let freq_step = self.freq_step.get();
                let delta = steps * freq_step as i32;

                if selection.is_empty()
                    && let Some(hovered) = hovered
                {
                    selection.insert(hovered);
                    self.selection_anchor.set(Some(hovered));
                    self.selection_cursor.set(Some(hovered));
                }

                let mut points = self.points.borrow_mut();
                let mut changed = false;
                for idx in selection.iter().filter(|idx| visible.contains(*idx)) {
                    shift_point_freq(&mut points[*idx], delta, freq_step, freq_range);
                    changed = true;
                }

                if changed {
                    APP_BROKER.send(AppMsg::SettingsChanged);
                }
            }
            VfCurveKeyAction::MoveCursor { delta, extend } => {
                let current = self
                    .selection_cursor
                    .get()
                    .filter(|cursor| selection.contains(cursor) && visible.contains(cursor))
                    .or(hovered);

                let len = self.points.borrow().len();
                let target = match current {
                    Some(current) => current.saturating_add_signed(delta).min(len - 1),
                    None if delta > 0 => visible_start,
                    None => visible_end - 1,
                };

                let target = if extend {
                    target.clamp(visible_start, visible_end - 1)
                } else {
                    // Scroll the view along when the cursor walks past its edge
                    if target < visible_start {
                        self.pan(target as isize - visible_start as isize);
                    } else if target >= visible_end {
                        self.pan((target + 1 - visible_end) as isize);
                    }
                    target
                };

                selection.clear();
                if extend {
                    let anchor = self
                        .selection_anchor
                        .get()
                        .filter(|anchor| visible.contains(anchor))
                        .or(current)
                        .unwrap_or(target);
                    selection.extend(cmp::min(anchor, target)..=cmp::max(anchor, target));
                    self.selection_anchor.set(Some(anchor));
                } else {
                    selection.insert(target);
                    self.selection_anchor.set(Some(target));
                }
                self.selection_cursor.set(Some(target));
            }
            VfCurveKeyAction::SelectAll => {
                selection.clear();
                selection.extend(visible.clone());
                self.selection_anchor.set(Some(visible_start));
                self.selection_cursor.set(Some(visible_end - 1));
            }
            VfCurveKeyAction::ClearSelection => {
                selection.clear();
                self.selection_anchor.set(None);
                self.selection_cursor.set(None);
            }
            VfCurveKeyAction::ZoomIn
            | VfCurveKeyAction::ZoomOut
            | VfCurveKeyAction::ResetZoom
            | VfCurveKeyAction::LineBetweenSelected => {}
        }
    }

    /// Outermost selected visible points, if there is anything between them to put on a line
    fn line_endpoints(&self) -> Option<(usize, usize)> {
        let (visible_start, visible_end) = self.visible_points_range();
        let selection = self.selected_points.borrow();
        let mut visible_selection = selection.range(visible_start..visible_end);

        let first = *visible_selection.next()?;
        let last = *visible_selection.next_back()?;
        (last - first > 1).then_some((first, last))
    }

    fn set_visible_points_range(&self, start: usize, end: usize) {
        let len = self.points.borrow().len();
        if len == 0 {
            return;
        }

        // Aim at the middle of a point's slot so that rounding cannot land on its neighbour
        let to_percentage = |idx: usize| {
            if idx == 0 {
                0.0
            } else if idx >= len {
                100.0
            } else {
                (idx as f64 + 0.5) * 100.0 / len as f64
            }
        };

        self.visible_range_start.set_value(to_percentage(start));
        self.visible_range_end.set_value(to_percentage(end));
    }

    /// Shifts the visible range by a number of points, keeping its width
    fn pan(&self, delta: isize) {
        let len = self.points.borrow().len();
        let (start, end) = self.visible_points_range();
        let width = end - start;
        if width == 0 || width >= len {
            return;
        }

        let new_start = start.saturating_add_signed(delta).min(len - width);
        self.set_visible_points_range(new_start, new_start + width);
    }

    /// Changes the number of visible points, keeping `focus` at the same place on screen
    fn zoom(&self, zoom_in: bool, focus: usize) {
        let len = self.points.borrow().len();
        let (start, end) = self.visible_points_range();
        let width = end - start;
        if width == 0 {
            return;
        }

        let min_width = cmp::min(MIN_VISIBLE_POINTS, len);
        let new_width = if zoom_in {
            cmp::min(width * 4 / 5, width.saturating_sub(1))
        } else {
            cmp::max(width * 5 / 4, width + 1)
        }
        .clamp(min_width, len);
        if new_width == width {
            return;
        }

        let focus = focus.clamp(start, end - 1);
        let focus_ratio = (focus - start) as f64 / width as f64;
        let new_start = (focus as f64 - focus_ratio * new_width as f64).round().max(0.0) as usize;
        let new_start = cmp::min(new_start, len - new_width);

        self.set_visible_points_range(new_start, new_start + new_width);
    }

    /// Point under the mouse pointer, or the middle of the view
    fn pointer_zoom_focus(&self) -> usize {
        let (start, end) = self.visible_points_range();
        let points = self.points.borrow();

        self.hovered_coords
            .get()
            .and_then(|(voltage, _)| {
                (start..end).min_by_key(|i| (points[*i].voltage as i64 - voltage as i64).abs())
            })
            .unwrap_or((start + end) / 2)
    }

    /// Point that the keyboard is on, or the middle of the view
    fn keyboard_zoom_focus(&self) -> usize {
        let (start, end) = self.visible_points_range();

        self.selection_cursor
            .get()
            .filter(|cursor| {
                (start..end).contains(cursor) && self.selected_points.borrow().contains(cursor)
            })
            .unwrap_or((start + end) / 2)
    }

    fn freq_limits_range(
        points: &[NvidiaVfPoint],
        offset_range: Option<(i32, i32)>,
    ) -> Option<(u32, u32)> {
        let (min_offset, max_offset) = offset_range?;
        let first_point = points.first()?;
        let last_point = points.last()?;

        let min_freq = offset_freq(first_point.base_freq, min_offset);
        let mut max_freq = offset_freq(last_point.base_freq, max_offset);

        if min_freq >= max_freq {
            max_freq = min_freq.saturating_add(1);
        }

        Some((min_freq, max_freq))
    }

    fn visible_freq_range(points: &[NvidiaVfPoint], freq_range: (u32, u32)) -> Option<(u32, u32)> {
        let min_freq = points
            .iter()
            .flat_map(|point| [point.freq, point.base_freq])
            .min()?;
        let max_freq = points
            .iter()
            .flat_map(|point| [point.freq, point.base_freq])
            .max()?;

        let y_start = min_freq
            .saturating_sub(VISIBLE_FREQ_RANGE_PADDING_MIN)
            .max(freq_range.0);
        let mut y_end = max_freq
            .saturating_add(VISIBLE_FREQ_RANGE_PADDING_MAX)
            .min(freq_range.1);
        if y_start >= y_end {
            y_end = y_start.saturating_add(1).min(freq_range.1);
        }

        Some((y_start, y_end))
    }

    pub fn is_empty(&self) -> bool {
        self.points.borrow().is_empty()
    }

    pub fn get_configured_curve(&self) -> IndexMap<u8, config::NvidiaCurvePoint> {
        let points = self.points.borrow();

        if !self.allow_editing.value() {
            return IndexMap::new();
        }

        points
            .iter()
            .map(|point| {
                let vf_point = config::NvidiaCurvePoint {
                    clockspeed_offset: point.freq_offset,
                    voltage: Some(point.voltage),
                };
                (point.index, vf_point)
            })
            .collect()
    }
}

fn vf_point_coords(point: &NvidiaVfPoint) -> (u32, u32) {
    (point.voltage, point.freq)
}

fn vf_point_base_coords(point: &NvidiaVfPoint) -> (u32, u32) {
    (point.base_voltage, point.base_freq)
}

fn set_point_freq(point: &mut NvidiaVfPoint, freq: u32) {
    point.freq = freq;
    point.freq_offset = freq as i32 - point.base_freq as i32;
}

fn shift_point_freq(point: &mut NvidiaVfPoint, delta: i32, freq_step: u32, freq_range: (u32, u32)) {
    let freq = (point.freq as i32 + delta).max(0) as u32;
    set_point_freq(point, snap_freq(freq, freq_step, freq_range));
}

/// Rounds a frequency to the clock grid of the card, staying inside of the allowed range
fn snap_freq(freq: u32, freq_step: u32, freq_range: (u32, u32)) -> u32 {
    let freq_step = freq_step.max(1);
    let min_freq = freq_range.0.div_ceil(freq_step) * freq_step;
    let max_freq = freq_range.1 / freq_step * freq_step;
    if min_freq > max_freq {
        return freq.clamp(freq_range.0, freq_range.1);
    }

    let snapped = (freq + freq_step / 2) / freq_step * freq_step;
    snapped.clamp(min_freq, max_freq)
}

/// The driver reports frequencies that are already rounded to its clock grid,
/// so the grid step is the greatest common divisor of the reported values
fn detect_freq_step(points: &[NvidiaVfPoint]) -> u32 {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }

    let step = points.iter().map(|point| point.freq).fold(0, gcd);
    if (1..=MAX_FREQ_STEP).contains(&step) {
        step
    } else {
        1
    }
}

fn key_action(key: gdk::Key, modifiers: gdk::ModifierType) -> Option<VfCurveKeyAction> {
    let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
    let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);

    let step = if ctrl { KEY_STEPS_COARSE } else { KEY_STEPS };

    let action = match key {
        gdk::Key::Up | gdk::Key::KP_Up => VfCurveKeyAction::Nudge(step),
        gdk::Key::Down | gdk::Key::KP_Down => VfCurveKeyAction::Nudge(-step),
        gdk::Key::Page_Up => VfCurveKeyAction::Nudge(KEY_STEPS_COARSE),
        gdk::Key::Page_Down => VfCurveKeyAction::Nudge(-KEY_STEPS_COARSE),
        gdk::Key::Left | gdk::Key::KP_Left => VfCurveKeyAction::MoveCursor {
            delta: -1,
            extend: shift,
        },
        gdk::Key::Right | gdk::Key::KP_Right => VfCurveKeyAction::MoveCursor {
            delta: 1,
            extend: shift,
        },
        gdk::Key::a | gdk::Key::A if ctrl => VfCurveKeyAction::SelectAll,
        gdk::Key::Escape => VfCurveKeyAction::ClearSelection,
        gdk::Key::l | gdk::Key::L => VfCurveKeyAction::LineBetweenSelected,
        gdk::Key::plus | gdk::Key::equal | gdk::Key::KP_Add => VfCurveKeyAction::ZoomIn,
        gdk::Key::minus | gdk::Key::KP_Subtract => VfCurveKeyAction::ZoomOut,
        gdk::Key::_0 | gdk::Key::KP_0 => VfCurveKeyAction::ResetZoom,
        _ => return None,
    };
    Some(action)
}

fn offset_freq(base_freq: u32, offset: i32) -> u32 {
    if offset.is_negative() {
        base_freq.saturating_sub(offset.unsigned_abs())
    } else {
        base_freq.saturating_add(offset as u32)
    }
}

fn curve_has_offsets(points: &[NvidiaVfPoint]) -> bool {
    points.iter().any(|point| point.freq_offset != 0)
}
