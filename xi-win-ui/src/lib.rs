// Copyright 2018 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Simple entity-component-system based GUI.

extern crate xi_win_shell;
extern crate direct2d;
extern crate directwrite;
extern crate winapi;

use std::any::Any;
use std::cell::RefCell;
use std::char;
use std::collections::BTreeMap;
use std::mem;
use std::ops::{Deref, DerefMut};

use direct2d::math::*;
use direct2d::RenderTarget;
use direct2d::render_target::GenericRenderTarget;
use direct2d::brush::SolidColorBrush;

use xi_win_shell::paint;
use xi_win_shell::win_main;
use xi_win_shell::window::{self, MouseType, WindowHandle, WinHandler};

pub mod widget;

pub use widget::{KeyEvent, KeyVariant, MouseEvent, Widget};

pub struct UiMain {
    state: RefCell<UiState>,
}

/// An identifier for widgets, scoped to a UiMain instance. This is the
/// "entity" of the entity-component-system architecture.
pub type Id = usize;

pub struct UiState {
    listeners: BTreeMap<Id, Vec<Box<FnMut(&mut Any, ListenerCtx)>>>,

    command_listener: Option<Box<FnMut(u32, ListenerCtx)>>,

    /// The widget tree and associated state is split off into a separate struct
    /// so that we can use a mutable reference to it as the listener context.
    inner: UiInner,
}

/// The context given to listeners.
///
/// Listeners are allowed to poke widgets and mutate the graph.
pub struct UiInner {
    /// The individual widget trait objects.
    widgets: Vec<Box<Widget>>,

    /// Graph of widgets (actually a strict tree structure, so maybe should be renamed).
    graph: Graph,

    /// The state (other than widget tree) is a separate object, so that a
    /// mutable reference to it can be used as a layout context.
    c: LayoutCtx,
}

/// The context given to layout methods.
pub struct LayoutCtx {
    dwrite_factory: directwrite::Factory,

    handle: WindowHandle,

    /// Bounding box of each widget. The position is relative to the parent.
    geom: Vec<Geometry>,

    /// Queue of events to distribute to listeners
    event_q: Vec<(Id, Box<Any>)>,

    /// Which widget is currently focused, if any.
    focused: Option<Id>,

    /// Which widget is active (mouse is pressed), if any.
    active: Option<Id>,

    /// Which widget is hot (hovered), if any.
    hot: Option<Id>,
}

#[derive(Default)]
struct Graph {
    root: Id,
    children: Vec<Vec<Id>>,
    parent: Vec<Id>,
}

#[derive(Default, Clone, Copy)]
pub struct Geometry {
    // Maybe PointF is a better type, then we could use the math from direct2d?
    pub pos: (f32, f32),
    pub size: (f32, f32),
}

#[derive(Clone, Copy)]
pub struct BoxConstraints {
    min_width: f32,
    max_width: f32,
    min_height: f32,
    max_height: f32,
}

pub enum LayoutResult {
    Size((f32, f32)),
    RequestChild(Id, BoxConstraints),
}

// Contexts for widget methods.

/// Context given to handlers.
pub struct HandlerCtx<'a> {
    /// The id of the node sending the event
    id: Id,

    c: &'a mut LayoutCtx,
}

pub struct ListenerCtx<'a> {
    id: Id,

    inner: &'a mut UiInner,
}

pub struct PaintCtx<'a, 'b: 'a>  {
    // TODO: maybe this should be a 3-way enum: normal/hot/active
    is_active: bool,
    is_hot: bool,
    inner: &'a mut paint::PaintCtx<'b>,
    dwrite_factory: &'a directwrite::Factory,
}

impl Geometry {
    fn offset(&self, offset: (f32, f32)) -> Geometry {
        Geometry {
            pos: (self.pos.0 + offset.0, self.pos.1 + offset.1),
            size: self.size
        }
    }
}

impl UiMain {
    pub fn new(state: UiState) -> UiMain {
        UiMain { state: RefCell::new(state) }
    }
}

impl UiState {
    pub fn new() -> UiState {
        UiState {
            listeners: Default::default(),
            command_listener: None,
            inner: UiInner {
                widgets: Vec::new(),
                graph: Default::default(),
                c: LayoutCtx {
                    dwrite_factory: directwrite::Factory::new().unwrap(),
                    geom: Vec::new(),
                    handle: Default::default(),
                    event_q: Vec::new(),
                    focused: None,
                    active: None,
                    hot: None,
                }
            }
        }
    }

    /// Add a listener that expects a specific type.
    pub fn add_listener<A, F>(&mut self, node: Id, mut f: F)
        where A: Any, F: FnMut(&mut A, ListenerCtx) + 'static
    {
        let wrapper: Box<FnMut(&mut Any, ListenerCtx)> = Box::new(move |a, ctx| {
            if let Some(arg) = a.downcast_mut() {
                f(arg, ctx)
            } else {
                println!("type mismatch in listener arg");
            }
        });
        self.listeners.entry(node).or_insert(Vec::new()).push(wrapper);
    }

    /// Set a listener for menu commands.
    pub fn set_command_listener<F>(&mut self, f: F)
        where F: FnMut(u32, ListenerCtx) + 'static
    {
        self.command_listener = Some(Box::new(f));
    }

    fn mouse(&mut self, x: f32, y: f32, raw_event: &window::MouseEvent) {
        fn dispatch_mouse(widgets: &mut [Box<Widget>], node: Id,
            x: f32, y: f32, raw_event: &window::MouseEvent, ctx: &mut HandlerCtx) -> bool
        {
            let count = if raw_event.ty == MouseType::Down { 1 } else { 0 };
            let event = MouseEvent {
                x, y,
                mods: raw_event.mods,
                which: raw_event.which,
                count,
            };
            widgets[node].mouse(&event, ctx)
        }

        fn mouse_rec(widgets: &mut [Box<Widget>], graph: &Graph,
            x: f32, y: f32, raw_event: &window::MouseEvent, ctx: &mut HandlerCtx)
            -> bool
        {
            let node = ctx.id;
            let g = ctx.c.geom[node];
            let x = x - g.pos.0;
            let y = y - g.pos.1;
            let mut handled = false;
            if x >= 0.0 && y >= 0.0 && x < g.size.0 && y < g.size.1 {
                handled = dispatch_mouse(widgets, node, x, y, raw_event, ctx);
                for child in graph.children[node].iter().rev() {
                    if handled {
                        break;
                    }
                    ctx.id = *child;
                    handled = mouse_rec(widgets, graph, x, y, raw_event, ctx);
                }
            }
            handled
        }

        if let Some(active) = self.c.active {
            // Send mouse event directly to active widget.
            let (x, y) = self.xy_to_local(active, x, y);
            dispatch_mouse(&mut self.inner.widgets, active, x, y, raw_event,
                &mut HandlerCtx {
                    id: active,
                    c: &mut self.inner.c,
                }
            );
        } else {
            mouse_rec(&mut self.inner.widgets, &self.inner.graph,
                x, y, raw_event,
                &mut HandlerCtx {
                    id: self.inner.graph.root,
                    c: &mut self.inner.c,
                }
            );
        }
        self.dispatch_events();
    }

    fn mouse_move(&mut self, x: f32, y: f32) {
        // Note: this logic is similar to that for hit testing on mouse, but is
        // slightly different if child geom's overlap. Maybe we reconcile them,
        // maybe it's fine.
        let mut node = self.graph.root;
        let mut new_hot = None;
        let (mut tx, mut ty) = (x, y);
        loop {
            let g = self.c.geom[node];
            tx -= g.pos.0;
            ty -= g.pos.1;
            if self.graph.children[node].is_empty() {
                new_hot = Some(node);
                break;
            }
            let mut child_hot = None;
            for child in self.graph.children[node].iter().rev() {
                let child_g = self.c.geom[*child];
                let cx = tx - child_g.pos.0;
                let cy = ty - child_g.pos.1;
                if cx >= 0.0 && cy >= 0.0 && cx < child_g.size.0 && cy < child_g.size.1 {
                    child_hot = Some(child);
                    break;
                }
            }
            if let Some(child) = child_hot {
                node = *child;
            } else {
                break;
            }
        }
        let old_hot = self.c.hot;
        if new_hot != old_hot {
            self.c.hot = new_hot;
            if let Some(old_hot) = old_hot {
                self.inner.widgets[old_hot].on_hot_changed(false,
                    &mut HandlerCtx {
                        id: old_hot,
                        c: &mut self.inner.c,
                    }
                );
            }
            if let Some(new_hot) = new_hot {
                self.inner.widgets[new_hot].on_hot_changed(true,
                    &mut HandlerCtx {
                        id: new_hot,
                        c: &mut self.inner.c,
                    }
                );
            }
        }

        if let Some(node) = self.c.active.or(new_hot) {
            let (x, y) = self.xy_to_local(node, x, y);
            self.inner.widgets[node].mouse_moved(x, y,
                &mut HandlerCtx {
                    id: node,
                    c: &mut self.inner.c,
                }
            );
        }
    }

    fn handle_key_event(&mut self, event: &KeyEvent) -> bool {
        if let Some(id) = self.c.focused {
            let handled = {
                let mut ctx = HandlerCtx {
                    id,
                    c: &mut self.inner.c,
                };
                self.inner.widgets[id].key(event, &mut ctx)
            };
            self.dispatch_events();
            handled
        } else {
            false
        }
    }

    fn handle_command(&mut self, cmd: u32) {
        if let Some(ref mut listener) = self.command_listener {
            let ctx = ListenerCtx {
                id: self.inner.graph.root,
                inner: &mut self.inner,
            };
            listener(cmd, ctx);
        } else {
            println!("command received but no handler");
        }
    }

    fn dispatch_events(&mut self) {
        while !self.c.event_q.is_empty() {
            let event_q = mem::replace(&mut self.c.event_q, Vec::new());
            for (id, mut event) in event_q {
                if let Some(listeners) = self.listeners.get_mut(&id) {
                    for listener in listeners {
                        let ctx = ListenerCtx {
                            id,
                            inner: &mut self.inner,
                        };
                        listener(event.deref_mut(), ctx);
                    }
                }
            }
        }
    }

    /// Translate coordinates to local coordinates of widget
    fn xy_to_local(&mut self, mut node: Id, mut x: f32, mut y: f32) -> (f32, f32) {
        loop {
            let g = self.c.geom[node];
            x -= g.pos.0;
            y -= g.pos.1;
            let parent = self.graph.parent[node];
            if parent == node {
                break;
            }
            node = parent;
        }
        (x, y)
    }
}

fn clamp(val: f32, min: f32, max: f32) -> f32 {
    if val < min {
        min
    } else if val > max {
        max
    } else {
        val
    }
}

impl Deref for UiState {
    type Target = UiInner;

    fn deref(&self) -> &UiInner {
        &self.inner
    }
}

impl DerefMut for UiState {
    fn deref_mut(&mut self) -> &mut UiInner {
        &mut self.inner
    }
}

impl UiInner {
    /// Send an arbitrary payload to a widget. The type and interpretation of the
    /// payload depends on the specific target widget.
    pub fn poke<A: Any>(&mut self, node: Id, payload: &mut A) -> bool {
        let mut ctx = HandlerCtx {
            id: node,
            c: &mut self.c,
        };
        self.widgets[node].poke(payload, &mut ctx)
    }

    /// Put a widget in the graph and add its children. Returns newly allocated
    /// id for the node.
    pub fn add<W>(&mut self, widget: W, children: &[Id]) -> Id
        where W: Widget + 'static
    {
        let id = self.graph.alloc_node();
        self.widgets.push(Box::new(widget));
        self.c.geom.push(Default::default());
        for &child in children {
            self.graph.append_child(id, child);
        }
        id
    }

    pub fn set_root(&mut self, root: Id) {
        self.graph.root = root;
    }

    /// Set the focused widget.
    pub fn set_focus(&mut self, node: Option<Id>) {
        self.c.focused = node;
    }

    // The following methods are really UiState methods, but don't need access to listeners
    // so are more concise to implement here.

    fn paint(&mut self, paint_ctx: &mut paint::PaintCtx, root: Id) {
        // Do pre-order traversal on graph, painting each node in turn.
        //
        // Implemented as a recursion, but we could use an explicit queue instead.
        fn paint_rec(widgets: &mut [Box<Widget>], graph: &Graph, geom: &[Geometry],
            paint_ctx: &mut PaintCtx, node: Id, pos: (f32, f32), active: Option<Id>,
            hot: Option<Id>)
        {
            let g = geom[node].offset(pos);
            paint_ctx.is_active = active == Some(node);
            paint_ctx.is_hot = hot == Some(node) && (paint_ctx.is_active || active.is_none());
            widgets[node].paint(paint_ctx, &g);
            for child in graph.children[node].clone() {
                paint_rec(widgets, graph, geom, paint_ctx, child, g.pos, active, hot);
            }
        }

        let mut paint_ctx = PaintCtx {
            is_active: false,
            is_hot: false,
            inner: paint_ctx,
            dwrite_factory: &self.c.dwrite_factory,
        };
        paint_rec(&mut self.widgets, &self.graph, &self.c.geom,
            &mut paint_ctx, root, (0.0, 0.0), self.c.active, self.c.hot);
    }

    fn layout(&mut self, bc: &BoxConstraints, root: Id) {
        fn layout_rec(widgets: &mut [Box<Widget>], ctx: &mut LayoutCtx, graph: &Graph,
            bc: &BoxConstraints, node: Id) -> (f32, f32)
        {
            let mut size = None;
            loop {
                let layout_res = widgets[node].layout(bc, &graph.children[node], size, ctx);
                match layout_res {
                    LayoutResult::Size(size) => {
                        ctx.geom[node].size = size;
                        return size;
                    }
                    LayoutResult::RequestChild(child, child_bc) => {
                        size = Some(layout_rec(widgets, ctx, graph, &child_bc, child));
                    }
                }
            }
        }

        layout_rec(&mut self.widgets, &mut self.c, &self.graph, bc, root);
    }
}

impl BoxConstraints {
    pub fn tight(size: (f32, f32)) -> BoxConstraints {
        BoxConstraints {
            min_width: size.0,
            max_width: size.0,
            min_height: size.1,
            max_height: size.1,
        }
    }

    pub fn constrain(&self, size: (f32, f32)) -> (f32, f32) {
        (clamp(size.0, self.min_width, self.max_width),
            clamp(size.1, self.min_height, self.max_height))
    }
}

impl LayoutCtx {
    pub fn dwrite_factory(&self) -> &directwrite::Factory {
        &self.dwrite_factory
    }

    pub fn position_child(&mut self, child: Id, pos: (f32, f32)) {
        self.geom[child].pos = pos;
    }

    pub fn get_child_size(&self, child: Id) -> (f32, f32) {
        self.geom[child].size
    }
}

impl<'a> HandlerCtx<'a> {
    /// Invalidate this widget. Finer-grained invalidation is not yet implemented,
    /// but when it is, this method will invalidate the widget's bounding box.
    pub fn invalidate(&self) {
        self.c.handle.invalidate();
    }

    /// Send an event, to be handled by listeners.
    pub fn send_event<A: Any>(&mut self, a: A) {
        self.c.event_q.push((self.id, Box::new(a)));
    }

    /// Set or unset the widget as active.
    // TODO: this should call SetCapture/ReleaseCapture as well.
    pub fn set_active(&mut self, active: bool) {
        self.c.active = if active { Some(self.id) } else { None };
    }

    /// Determine whether this widget is active.
    pub fn is_active(&self) -> bool {
        self.c.active == Some(self.id)
    }

    /// Determine whether this widget is hot. A widget can be both hot and active, but
    /// if a widget is active, it is the only widget that can be hot.
    pub fn is_hot(&self) -> bool {
        self.c.hot == Some(self.id) && (self.is_active() || self.c.active.is_none())
    }
}

impl<'a> Deref for ListenerCtx<'a> {
    type Target = UiInner;

    fn deref(&self) -> &UiInner {
        self.inner
    }
}

impl<'a> DerefMut for ListenerCtx<'a> {
    fn deref_mut(&mut self) -> &mut UiInner {
        self.inner
    }
}

impl<'a> ListenerCtx<'a> {
    /// Bubble a poke action up the widget hierarchy, until a widget handles it.
    ///
    /// Returns true if any widget handled the action.
    pub fn poke_up<A: Any>(&mut self, payload: &mut A) -> bool {
        let mut node = self.id;
        loop {
            let parent = self.graph.parent[node];
            if parent == node {
                return false;
            }
            node = parent;
            if self.poke(node, payload) {
                return true;
            }
        }
    }

    /// Request the window to be closed.
    pub fn close(&mut self) {
        self.c.handle.close();
    }
}

impl<'a, 'b> PaintCtx<'a, 'b> {
    pub fn dwrite_factory(&self) -> &directwrite::Factory {
        self.dwrite_factory
    }

    pub fn render_target(&mut self) -> &mut GenericRenderTarget {
        self.inner.render_target()
    }

    /// Determine whether this widget is the active one.
    pub fn is_active(&self) -> bool {
        self.is_active
    }

    /// Determine whether this widget is hot.
    pub fn is_hot(&self) -> bool {
        self.is_hot
    }
}

impl WinHandler for UiMain {
    fn connect(&self, handle: &WindowHandle) {
        self.state.borrow_mut().c.handle = handle.clone();
    }

    fn paint(&self, paint_ctx: &mut paint::PaintCtx) -> bool {
        let size;
        {
            let rt = paint_ctx.render_target();
            size = rt.get_size();
            let rect = RectF::from((0.0, 0.0, size.width, size.height));
            let bg = SolidColorBrush::create(rt).with_color(0x272822).build().unwrap();
            rt.fill_rectangle(rect, &bg);
        }
        let mut state = self.state.borrow_mut();
        let root = state.graph.root;
        let bc = BoxConstraints::tight((size.width, size.height));
        // TODO: be lazier about relayout
        state.layout(&bc, root);
        state.paint(paint_ctx, root);
        false
    }

    fn command(&self, id: u32) {
        // TODO: plumb through to client
        let mut state = self.state.borrow_mut();
        state.handle_command(id);
    }

    fn char(&self, ch: u32, mods: u32) {
        if let Some(ch) = char::from_u32(ch) {
            let key_event = KeyEvent { key: KeyVariant::Char(ch), mods };
            let mut state = self.state.borrow_mut();
            state.handle_key_event(&key_event);
        } else {
            println!("invalid code point 0x{:x}", ch);
        }
    }

    fn keydown(&self, vk_code: i32, mods: u32) -> bool {
        let key_event = KeyEvent { key: KeyVariant::Vkey(vk_code), mods };
        let mut state = self.state.borrow_mut();
        state.handle_key_event(&key_event)
    }

    fn mouse_wheel(&self, delta: i32, mods: u32) {
        println!("mouse_wheel {} {:02x}", delta, mods);
    }

    fn mouse_hwheel(&self, delta: i32, mods: u32) {
        println!("mouse_hwheel {} {:02x}", delta, mods);
    }

    fn mouse_move(&self, x: i32, y: i32, _mods: u32) {
        let mut state = self.state.borrow_mut();
        let (x, y) = state.c.handle.pixels_to_px_xy(x, y);
        state.mouse_move(x, y);
    }

    fn mouse(&self, event: &window::MouseEvent) {
        println!("mouse {:?}", event);
        let mut state = self.state.borrow_mut();
        let (x, y) = state.c.handle.pixels_to_px_xy(event.x, event.y);
        // TODO: detect multiple clicks and pass that down
        state.mouse(x, y, event);
    }

    fn destroy(&self) {
        win_main::request_quit();
    }

    fn as_any(&self) -> &Any { self }
}

impl Graph {
    pub fn alloc_node(&mut self) -> Id {
        let id = self.children.len();
        self.children.push(vec![]);
        self.parent.push(id);
        id
    }

    pub fn append_child(&mut self, parent: Id, child: Id) {
        self.children[parent].push(child);
        self.parent[child] = parent;
    }
}
