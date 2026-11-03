use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Renderer as _, Shell, Widget, mouse};
use iced::animation::Easing;
use iced::core::widget::tree;
use iced::widget::canvas::{self, Canvas, Frame, Geometry, Path};
use iced::{
    Animation, Background, Border, Color, Length, Padding, Point, Rectangle, Shadow, Size, Vector,
    alignment, event, overlay, touch,
};
use std::time::{Duration, Instant};

use crate::components::menu::ANIMATION_DURATION;

type Renderer = iced::Renderer;
type Element<'a, Message, Theme> = iced::core::Element<'a, Message, Theme, Renderer>;

/// Draws the menu as one shape with the bar: the panel grows out of the bar
/// edge on a spring, and concave fillets join its sides to the bar.
#[derive(Debug, Clone, Copy)]
pub struct Fluid {
    /// Fill of the panel and its fillets, matching the bar.
    pub color: Color,
    /// Radius of the far corners and resting radius of the fillets.
    pub radius: f32,
}

/// How far the fillets widen at full speed, as a multiple of their resting radius.
const FILLET_STRETCH: f32 = 1.2;
/// Panel heights per second at which the fillets reach their full stretch.
const FILLET_SPEED: f32 = 6.0;
/// Pixels per second along the bar at which the fillets reach their full stretch.
const FILLET_SLIDE_SPEED: f32 = 1500.0;
/// Opening overshoots a little and settles back, closing does not.
const OPEN_SPRING: Spring = Spring {
    stiffness: 380.0,
    damping: 24.0,
};
const CLOSE_SPRING: Spring = Spring {
    stiffness: 500.0,
    damping: 40.0,
};
/// Moves the panel along the bar and resizes it when another menu takes its place.
const SLIDE_SPRING: Spring = Spring {
    stiffness: 260.0,
    damping: 28.0,
};
/// Kappa for a quarter circle drawn as a cubic Bézier.
const KAPPA: f32 = 0.552_284_8;

#[derive(Clone, Copy)]
struct Spring {
    stiffness: f32,
    damping: f32,
}

/// One value moving on a spring.
#[derive(Default, Clone, Copy)]
struct Axis {
    position: f32,
    velocity: f32,
}

impl Axis {
    fn at(position: f32) -> Self {
        Self {
            position,
            velocity: 0.0,
        }
    }

    fn settled(&self, target: f32, tolerance: f32) -> bool {
        (self.position - target).abs() < tolerance && self.velocity.abs() < tolerance * 10.0
    }

    /// Advances by `seconds` in fixed substeps, so a slow frame cannot make the
    /// spring unstable.
    fn advance(&mut self, target: f32, spring: Spring, seconds: f32) {
        const SUBSTEP: f32 = 1.0 / 240.0;
        let mut remaining = seconds;
        while remaining > 0.0 {
            let dt = remaining.min(SUBSTEP);
            let accel =
                spring.stiffness * (target - self.position) - spring.damping * self.velocity;
            self.velocity += accel * dt;
            self.position += self.velocity * dt;
            remaining -= dt;
        }
    }
}

/// Where the fluid panel is drawn. It follows the content's layout on a
/// spring, so switching to another menu slides the panel along the bar and
/// resizes it.
#[derive(Clone, Copy)]
struct Panel {
    x: Axis,
    width: Axis,
    height: Axis,
}

impl Panel {
    fn at(bounds: Rectangle) -> Self {
        Self {
            x: Axis::at(bounds.x),
            width: Axis::at(bounds.width),
            height: Axis::at(bounds.height),
        }
    }

    fn settled(&self, target: Rectangle) -> bool {
        self.x.settled(target.x, 0.5)
            && self.width.settled(target.width, 0.5)
            && self.height.settled(target.height, 0.5)
    }
}

/// The fluid style's motion: how far the panel is out of the bar, 0 hidden and
/// 1 at full height with its velocity in heights per second, and the panel's
/// place along the bar.
#[derive(Default)]
struct Motion {
    extension: Axis,
    panel: Option<Panel>,
    target: Rectangle,
    last: Option<Instant>,
}

impl Motion {
    fn settled(&self, open: f32) -> bool {
        self.extension.settled(open, 0.001)
            && self.panel.is_none_or(|panel| panel.settled(self.target))
    }

    fn step(&mut self, open: f32, now: Instant) {
        let seconds = self
            .last
            .map_or(Duration::from_millis(16), |last| {
                now.saturating_duration_since(last)
            })
            .min(Duration::from_millis(50))
            .as_secs_f32();
        self.last = Some(now);

        let spring = if open > 0.5 {
            OPEN_SPRING
        } else {
            CLOSE_SPRING
        };
        self.extension.advance(open, spring, seconds);
        // Closing stops at the bar instead of passing through it.
        if open < 0.5 && self.extension.position <= 0.0 {
            self.extension = Axis::at(0.0);
        }

        let target = self.target;
        if let Some(panel) = &mut self.panel {
            panel.x.advance(target.x, SLIDE_SPRING, seconds);
            panel.width.advance(target.width, SLIDE_SPRING, seconds);
            panel.height.advance(target.height, SLIDE_SPRING, seconds);
        }

        if self.settled(open) {
            self.extension = Axis::at(open);
            self.panel = Some(Panel::at(target));
            self.last = None;
        }
    }

    /// How fast the panel moves, 1 at the speed where the fillets stretch fully.
    fn speed(&self) -> f32 {
        let along = self
            .panel
            .map_or(0.0, |panel| panel.x.velocity.abs() / FILLET_SLIDE_SPEED);
        (self.extension.velocity.abs() / FILLET_SPEED)
            .max(along)
            .min(1.0)
    }

    /// The panel's bounds for this frame, at full extension.
    fn bounds(&self) -> Rectangle {
        self.panel.map_or(self.target, |panel| Rectangle {
            x: panel.x.position,
            y: self.target.y,
            width: panel.width.position.max(0.0),
            height: panel.height.position.max(0.0),
        })
    }
}

struct State {
    progress_anim: Animation<f32>,
    motion: Motion,
    last_open: bool,
    initialized: bool,
}

#[allow(missing_debug_implementations)]
pub struct MenuWrapper<'a, Message, Theme = iced::Theme> {
    x: f32,
    content: Element<'a, Message, Theme>,
    on_click_outside: Option<Message>,
    padding: Padding,
    vertical_alignment: alignment::Vertical,
    backdrop: Option<Color>,
    open: bool,
    animated: bool,
    fluid: Option<Fluid>,
}

impl<'a, Message, Theme> MenuWrapper<'a, Message, Theme> {
    pub fn new(x: f32, content: Element<'a, Message, Theme>) -> Self {
        MenuWrapper {
            x,
            content,
            on_click_outside: None,
            vertical_alignment: alignment::Vertical::Top,
            padding: Padding::ZERO,
            backdrop: None,
            open: true,
            animated: true,
            fluid: None,
        }
    }

    pub fn padding<P: Into<Padding>>(mut self, padding: P) -> Self {
        self.padding = padding.into();
        self
    }

    pub fn align_y(mut self, alignment: impl Into<alignment::Vertical>) -> Self {
        self.vertical_alignment = alignment.into();
        self
    }

    pub fn on_click_outside(mut self, message: Message) -> Self {
        self.on_click_outside = Some(message);
        self
    }

    pub fn backdrop(mut self, color: Color) -> Self {
        self.backdrop = Some(color);
        self
    }

    pub fn open(mut self, open: bool) -> Self {
        self.open = open;
        self
    }

    pub fn animated(mut self, animated: bool) -> Self {
        self.animated = animated;
        self
    }

    /// Draws the menu joined to the bar. Only takes effect while animated.
    pub fn fluid(mut self, fluid: Option<Fluid>) -> Self {
        self.fluid = fluid;
        self
    }

    fn slide_from_top(&self) -> bool {
        matches!(self.vertical_alignment, alignment::Vertical::Top)
    }

    fn fluid_style(&self) -> Option<Fluid> {
        self.fluid.filter(|_| self.animated)
    }

    fn target(&self) -> f32 {
        if self.open { 1.0 } else { 0.0 }
    }
}

impl<'a, Message, Theme> Widget<Message, Theme, Renderer> for MenuWrapper<'a, Message, Theme>
where
    Message: Clone,
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            progress_anim: Animation::new(0.0),
            motion: Motion::default(),
            last_open: false,
            initialized: false,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: Length::Fill,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let fluid = self.fluid_style();
        let node = layout::positioned(
            limits,
            Length::Fill,
            Length::Fill,
            self.padding,
            |limits| {
                self.content
                    .as_widget_mut()
                    .layout(&mut tree.children[0], renderer, limits)
            },
            |node, size| {
                let content_size = node.size();
                let centered = self.x - content_size.width / 2.0;
                let x = match fluid {
                    // A fillet needs room beside the panel. Where there is
                    // none, the panel goes flush with the screen edge and
                    // that side has no fillet.
                    Some(fluid) => {
                        let room = fluid.radius * (1.0 + FILLET_STRETCH);
                        let right = size.width - content_size.width;
                        if centered < room {
                            0.0
                        } else if centered > right - room {
                            right.max(0.0)
                        } else {
                            centered
                        }
                    }
                    None => f32::min(f32::max(centered, 8.), size.width - content_size.width - 8.),
                };
                let node = node.align(
                    iced::Alignment::Center,
                    self.vertical_alignment.into(),
                    size,
                );
                let y = node.bounds().y;
                node.move_to(Point::new(x, y))
            },
        );

        let state = tree.state.downcast_mut::<State>();
        let now = Instant::now();

        // The panel starts where the content is, and slides from there when
        // the content moves while the panel is out.
        let content = node.children()[0].bounds();
        state.motion.target = content;
        if fluid.is_none() || state.motion.panel.is_none() || state.motion.extension.position < 0.01
        {
            state.motion.panel = Some(Panel::at(content));
        }

        if !self.animated {
            let target = self.target();
            state.progress_anim = Animation::new(target);
            state.motion.extension = Axis::at(target);
            state.last_open = self.open;
            state.initialized = true;
        } else if !state.initialized {
            let initial = if self.open { 0.0 } else { 1.0 };
            state.progress_anim = Animation::new(initial)
                .duration(ANIMATION_DURATION)
                .easing(Easing::EaseOutCubic);
            state.motion.extension = Axis::at(initial);
            state.last_open = self.open;
            state.initialized = true;
            if self.open {
                state.progress_anim.go_mut(1.0, now);
            }
        } else if self.open != state.last_open {
            state.last_open = self.open;
            state.progress_anim.go_mut(self.target(), now);
        }

        node
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout.children().next().unwrap(),
                renderer,
                operation,
            );
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &event::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.children().next().unwrap(),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if let event::Event::Window(iced::core::window::Event::RedrawRequested(now)) = event {
            let target = self.target();
            let fluid = self.fluid_style().is_some();
            let state = tree.state.downcast_mut::<State>();
            if fluid {
                if !state.motion.settled(target) {
                    state.motion.step(target, *now);
                    shell.request_redraw();
                }
            } else if state.progress_anim.is_animating(*now) {
                shell.request_redraw();
                shell.invalidate_layout();
            }
        }

        // Ignore click-outside while the close animation plays — otherwise a
        // late click could re-emit CloseMenu and trigger spurious work.
        if !self.open {
            return;
        }

        if let Some(on_click_outside) = &self.on_click_outside
            && let event::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
            | event::Event::Touch(touch::Event::FingerLifted { .. }) = event
        {
            let bounds = layout.children().next().unwrap().bounds();
            let cursor_over_scrollable = cursor.is_over(bounds);
            if !cursor_over_scrollable {
                shell.publish(on_click_outside.clone());
            }
        }
    }

    fn mouse_interaction(
        &self,
        _: &Tree,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
        _: &Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::default()
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        renderer_style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let now = Instant::now();
        let fluid = self.fluid_style();
        let progress = if fluid.is_some() {
            state.motion.extension.position
        } else if state.progress_anim.is_animating(now) {
            state.progress_anim.interpolate_with(|v| v, now)
        } else if self.open {
            1.0
        } else {
            0.0
        };

        if let Some(backdrop) = self.backdrop {
            let mut backdrop = backdrop;
            backdrop.a *= progress.clamp(0.0, 1.0);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: Border::default(),
                    shadow: Shadow::default(),
                    snap: true,
                },
                Background::Color(backdrop),
            );
        }

        if progress < 0.01 {
            return;
        }

        let content_layout = layout.children().next().unwrap();
        let content_bounds = content_layout.bounds();
        let full_height = content_bounds.height;
        // The fluid panel may still be sliding towards the content.
        let panel_bounds = if fluid.is_some() {
            state.motion.bounds()
        } else {
            content_bounds
        };
        let panel_height = panel_bounds.height;
        let visible_height = panel_height * progress;

        // Clip-reveal: content is drawn at full size, but a growing clip rect
        // hides everything past `progress * height`. Anchored to the bar edge
        // so the menu "rolls out" from there. The fluid style also slides the
        // content out with the panel and may overshoot the full height.
        let (clip_bounds, offset) = if self.slide_from_top() {
            let clip = Rectangle {
                x: panel_bounds.x,
                y: content_bounds.y,
                width: panel_bounds.width,
                height: visible_height,
            };
            (clip, visible_height - full_height)
        } else {
            let clip = Rectangle {
                x: panel_bounds.x,
                y: content_bounds.y + full_height - visible_height,
                width: panel_bounds.width,
                height: visible_height,
            };
            (clip, full_height - visible_height)
        };

        let offset = if let Some(fluid) = fluid {
            let canvas: Canvas<_, Message, Theme, Renderer> = Canvas::new(Silhouette {
                path: silhouette(
                    fluid,
                    layout.bounds(),
                    clip_bounds,
                    self.slide_from_top(),
                    state.motion.speed(),
                ),
                color: fluid.color,
            })
            .width(Length::Fill)
            .height(Length::Fill);
            let canvas_tree = Tree::new(&canvas as &dyn Widget<Message, Theme, Renderer>);
            canvas.draw(
                &canvas_tree,
                renderer,
                theme,
                renderer_style,
                layout,
                cursor,
                viewport,
            );
            offset
        } else {
            0.0
        };

        // The layer clip is invisible to children; `viewport` is what they read.
        // They draw shifted by the slide and the extension, so their viewport
        // is shifted back, or a child that clips itself, like a scrollable,
        // would show outside the panel.
        let Some(child_viewport) = clip_bounds.intersection(viewport) else {
            return;
        };
        let slide = panel_bounds.x - content_bounds.x;
        let child_viewport = child_viewport - Vector::new(slide, offset);
        renderer.with_layer(clip_bounds, |renderer| {
            renderer.with_translation(Vector::new(slide, offset), |renderer| {
                self.content.as_widget().draw(
                    &tree.children[0],
                    renderer,
                    theme,
                    renderer_style,
                    content_layout,
                    cursor,
                    &child_viewport,
                );
            });
        });
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.children().next().unwrap(),
            renderer,
            viewport,
            translation,
        )
    }
}

/// The panel and its fillets as one filled shape.
struct Silhouette {
    path: Path,
    color: Color,
}

impl<Message, Theme> canvas::Program<Message, Theme, Renderer> for Silhouette {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<Renderer>> {
        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill(&self.path, self.color);
        vec![frame.into_geometry()]
    }
}

/// The outline of the panel and the fillets that join it to the bar edge, in
/// the coordinates of the menu surface. `panel` is the visible part of the
/// menu, and `screen` the whole surface.
fn silhouette(
    fluid: Fluid,
    screen: Rectangle,
    panel: Rectangle,
    from_top: bool,
    speed: f32,
) -> Path {
    let extent = panel.height;
    let (left, right) = (panel.x - screen.x, panel.x + panel.width - screen.x);
    let flush_left = left <= 0.5;
    let flush_right = right >= screen.width - 0.5;

    // The fillets widen with the speed of the panel and settle back with it.
    let stretch = 1.0 + FILLET_STRETCH * speed;
    let fillet = (fluid.radius * stretch).min(extent);
    let fillet_left = if flush_left { 0.0 } else { fillet.min(left) };
    let fillet_right = if flush_right {
        0.0
    } else {
        fillet.min(screen.width - right)
    };
    let corner = fluid.radius.min(extent / 2.0).min(panel.width / 2.0);
    // Corners square off as the panel reaches a screen edge, so a panel
    // sliding away from one rounds its corner gradually.
    let corner_left = corner.min(left.max(0.0));
    let corner_right = corner.min((screen.width - right).max(0.0));

    // Distance from the bar edge maps to y, towards the screen's far side.
    let edge = if from_top { 0.0 } else { screen.height };
    let point =
        |x: f32, depth: f32| Point::new(x, if from_top { edge + depth } else { edge - depth });

    Path::new(|p| {
        p.move_to(point(left - fillet_left, 0.0));
        p.bezier_curve_to(
            point(left - fillet_left * (1.0 - KAPPA), 0.0),
            point(left, fillet_left * (1.0 - KAPPA)),
            point(left, fillet_left),
        );
        p.line_to(point(left, extent - corner_left));
        p.bezier_curve_to(
            point(left, extent - corner_left * (1.0 - KAPPA)),
            point(left + corner_left * (1.0 - KAPPA), extent),
            point(left + corner_left, extent),
        );
        p.line_to(point(right - corner_right, extent));
        p.bezier_curve_to(
            point(right - corner_right * (1.0 - KAPPA), extent),
            point(right, extent - corner_right * (1.0 - KAPPA)),
            point(right, extent - corner_right),
        );
        p.line_to(point(right, fillet_right));
        p.bezier_curve_to(
            point(right, fillet_right * (1.0 - KAPPA)),
            point(right + fillet_right * (1.0 - KAPPA), 0.0),
            point(right + fillet_right, 0.0),
        );
        p.close();
    })
}

impl<'a, Message, Theme> From<MenuWrapper<'a, Message, Theme>> for Element<'a, Message, Theme>
where
    Message: 'a + Clone,
    Theme: 'a,
{
    fn from(menu_wrapper: MenuWrapper<'a, Message, Theme>) -> Self {
        Self::new(menu_wrapper)
    }
}
