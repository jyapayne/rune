//! Wheels over elements that name the viewport they scroll (`El::wheel_scrolls`): a scroll bar
//! drawn beside the viewport it follows, which the chain up the ancestors can't find.
use super::*;

use crate::{Env, ScrollSpec, col, el};

/// The offset of the element at `path`.
fn offset(rt: &Runtime<()>, path: &str) -> f64 {
    let id = rt.tree.nodes.iter().find(|n| n.path == path).unwrap_or_else(|| panic!("{path}")).id;
    rt.scroll.scrolls[&id].offset
}

/// A bar beside its viewport, as `rune-kit`'s scroll bar is: in an `area` column with the
/// viewport, drawn after it, so a wheel over the bar hits it and the viewport is a sibling.
/// The named one hands its wheel to the viewport (`El::wheel_scrolls`); the plain one names
/// nothing, as any box taking input does.
fn barred(named: bool) -> El {
    let bar = el().key("bar").absolute(286.0, 0.0).size(14.0, 200.0).on_drag(|_: &mut (), _, _| {});
    let bar = if named { bar.wheel_scrolls("area/list") } else { bar };
    el().key("root").w_full().h_full().child(
        col().key("area").size(300.0, 200.0)
            .child(col().key("list").w_full().h_full().scroll(ScrollSpec { stick_bottom: false })
                .child(el().w_full().h(1000.0)))
            .child(bar),
    )
}

fn named_bar(_: &(), _: &mut crate::Cx<()>) -> El { barred(true) }
fn plain_bar(_: &(), _: &mut crate::Cx<()>) -> El { barred(false) }

/// A wheel over an element naming the viewport it scrolls (`El::wheel_scrolls`) is that
/// viewport's, though the viewport is its sibling drawn under it: an overlay scroll bar's
/// track beside the viewport it follows.
#[test]
fn a_wheel_over_an_element_that_names_its_viewport_scrolls_that_viewport() {
    let mut rt = Runtime::new((), named_bar, (300.0, 200.0), Env::default());
    rt.frame(0.0);
    rt.wheel(293.0, 100.0, -30.0);
    rt.frame(0.0);
    assert_eq!(offset(&rt, "root/area/list"), 30.0, "the wheel over the bar scrolled the list it follows");

    // Unnamed, the same bar leaves the wheel with what lies above it, as any box taking input
    // does: the list is a sibling, not an ancestor, so it never sees the wheel.
    let mut plain = Runtime::new((), plain_bar, (300.0, 200.0), Env::default());
    plain.frame(0.0);
    plain.wheel(293.0, 100.0, -30.0);
    plain.frame(0.0);
    assert_eq!(offset(&plain, "root/area/list"), 0.0, "a bar that names nothing scrolls nothing");
}

/// A wheel over a bar takes the named viewport before the page around it, and chains on up from
/// that viewport when it has no room, as one over the viewport itself does.
#[test]
fn a_wheel_a_named_viewport_cannot_take_chains_up_from_it() {
    fn paged(content: f32) -> El {
        el().key("root").w_full().h_full().child(
            col().key("page").size(300.0, 200.0).scroll(ScrollSpec { stick_bottom: false })
                .child(el().key("body").w_full().h(1000.0).child(
                    col().key("area").size(300.0, 200.0)
                        .child(col().key("list").w_full().h_full().scroll(ScrollSpec { stick_bottom: false })
                            .child(el().w_full().h(content)))
                        .child(el().key("bar").absolute(286.0, 0.0).size(14.0, 200.0)
                            .on_drag(|_: &mut (), _, _| {}).wheel_scrolls("area/list")),
                )),
        )
    }
    fn page_tall(_:&(), _: &mut crate::Cx<()>) -> El { paged(1000.0) }
    fn page_short(_:&(), _: &mut crate::Cx<()>) -> El { paged(100.0) }

    let mut tall = Runtime::new((), page_tall, (300.0, 200.0), Env::default());
    tall.frame(0.0);
    tall.wheel(293.0, 100.0, -30.0);
    tall.frame(0.0);
    assert_eq!(offset(&tall, "root/page/body/area/list"), 30.0, "the named viewport takes the wheel before the page around it");
    assert_eq!(offset(&tall, "root/page"), 0.0, "the page around it stays where it is");

    let mut short = Runtime::new((), page_short, (300.0, 200.0), Env::default());
    short.frame(0.0);
    short.wheel(293.0, 100.0, -30.0);
    short.frame(0.0);
    assert_eq!(offset(&short, "root/page"), 30.0, "a short list hands the wheel on to the page around it");
    assert_eq!(offset(&short, "root/page/body/area/list"), 0.0, "the list itself never moved");
}

/// A raised panel over a bar keeps the wheel from the viewport under it, as #1137 chains a
/// wheel from what takes input or paints above: naming a viewport is how a bar belongs to
/// one, not a way through a panel over it.
#[test]
fn a_raised_panel_over_a_bar_keeps_the_wheel_from_the_viewport_under_it() {
    fn panel(_:&(), _: &mut crate::Cx<()>) -> El {
        el().key("root").w_full().h_full().child(
            col().key("area").size(300.0, 200.0)
                .child(col().key("list").w_full().h_full().scroll(ScrollSpec { stick_bottom: false })
                    .child(el().w_full().h(1000.0)))
                .child(el().key("bar").absolute(286.0, 0.0).size(14.0, 200.0)
                    .on_drag(|_: &mut (), _, _| {}).wheel_scrolls("area/list"))
                .child(el().key("panel").absolute(280.0, 0.0).size(20.0, 100.0).raise()),
        )
    }
    let mut rt = Runtime::new((), panel, (300.0, 200.0), Env::default());
    rt.frame(0.0);
    rt.wheel(293.0, 60.0, -30.0);
    rt.frame(0.0);
    assert_eq!(offset(&rt, "root/area/list"), 0.0, "the panel over the bar takes the wheel, as it did before");
}

/// Two panes keying their list and bar alike, as a chat and a sidebar do: the wheel over the
/// second pane's bar is its own, not the first element keyed `area/list` in the window.
#[test]
fn a_wheel_over_one_pane_s_bar_scrolls_that_pane_when_the_panes_key_alike() {
    fn panes(_: &(), _: &mut crate::Cx<()>) -> El {
        let pane = |key: &str| col().key(key.to_owned()).w(150.0).h(200.0).child(
            col().key("area").w_full().h_full()
                .child(col().key("list").w_full().h_full().scroll(ScrollSpec { stick_bottom: false })
                    .child(el().w_full().h(1000.0)))
                .child(el().key("bar").absolute(136.0, 0.0).size(14.0, 200.0)
                    .on_drag(|_: &mut (), _, _| {}).wheel_scrolls("area/list")),
        );
        crate::row().key("root").w_full().h_full().child(pane("left")).child(pane("right"))
    }
    let mut rt = Runtime::new((), panes, (300.0, 200.0), Env::default());
    rt.frame(0.0);
    assert_eq!((offset(&rt, "root/left/area/list"), offset(&rt, "root/right/area/list")), (0.0, 0.0));
    // The second pane's bar: its strip over the pane's right edge, at x 286..300.
    rt.wheel(293.0, 100.0, -30.0);
    rt.frame(0.0);
    assert_eq!(offset(&rt, "root/right/area/list"), 30.0, "the wheel over the right pane's bar scrolled its own list");
    assert_eq!(offset(&rt, "root/left/area/list"), 0.0, "the left pane, keyed alike, stayed where it was");
}

/// A sideways wheel over a bar is the viewport the bar follows too: a two-way pane's vertical
/// track scrolls it across, and a table's horizontal track scrolls the table itself, where a
/// chain up from either track finds no horizontal scroller.
#[test]
fn a_sideways_wheel_over_a_bar_scrolls_the_viewport_it_follows() {
    fn both_ways(_: &(), _: &mut crate::Cx<()>) -> El {
        el().key("root").w_full().h_full().child(
            col().key("area").size(300.0, 200.0)
                .child(col().key("list").w_full().h_full().scroll_xy().child(el().size(900.0, 1000.0)))
                .child(el().key("bar").absolute(286.0, 0.0).size(14.0, 200.0)
                    .on_drag(|_: &mut (), _, _| {}).wheel_scrolls("area/list")),
        )
    }
    fn sideways(_: &(), _: &mut crate::Cx<()>) -> El {
        el().key("root").w_full().h_full().child(
            col().key("area").size(300.0, 200.0)
                .child(col().key("table").w_full().h(100.0).scroll_x().child(el().size(900.0, 100.0)))
                .child(el().key("bar").absolute(0.0, 102.0).size(300.0, 8.0)
                    .on_drag(|_: &mut (), _, _| {}).wheel_scrolls("area/table")),
        )
    }

    // A vertical bar over a viewport that scrolls both ways.
    let mut across = Runtime::new((), both_ways, (300.0, 200.0), Env::default());
    across.frame(0.0);
    across.wheel_xy(293.0, 100.0, -40.0, 0.0);
    across.frame(0.0);
    {
        let id = across.tree.nodes.iter().find(|n| n.path == "root/area/list").unwrap().id;
        assert_eq!(across.scroll.scrolls[&id].x.offset, 40.0, "the sideways wheel over the pane's bar scrolled it across");
        assert_eq!(across.scroll.scrolls[&id].offset, 0.0, "and left it where it was down");
    }

    // A horizontal bar under the table it belongs to.
    let mut along = Runtime::new((), sideways, (300.0, 200.0), Env::default());
    along.frame(0.0);
    along.wheel_xy(150.0, 106.0, -40.0, 0.0);
    along.frame(0.0);
    {
        let id = along.tree.nodes.iter().find(|n| n.path == "root/area/table").unwrap().id;
        assert_eq!(along.scroll.scrolls[&id].x.offset, 40.0, "the sideways wheel over the table's own bar scrolled the table");
    }
}

/// A root that hears the back and forward buttons, over a row that takes presses and a canvas
/// that takes the back button itself.
#[derive(Default)]
struct Buttons { back: u32, forward: u32, row: u32, canvas: u32 }

fn buttons(_: &Buttons, _: &mut crate::Cx<Buttons>) -> El {
    el().key("root").size(300.0, 200.0)
        .on_button_down([PointerButton::Back, PointerButton::Forward], |s: &mut Buttons, tx| match tx.button() {
            PointerButton::Back => s.back += 1,
            _ => s.forward += 1,
        })
        .child(el().key("row").size(100.0, 40.0).on_press(|s: &mut Buttons| s.row += 1))
        .child(el().key("canvas").absolute(200.0, 0.0).size(100.0, 100.0).pointer_buttons([PointerButton::Back]).on_press(|s: &mut Buttons| s.canvas += 1))
}

#[test]
fn on_button_down_hears_its_buttons_over_descendants_that_do_not_take_them() {
    let mut rt = Runtime::new(Buttons::default(), buttons, (300.0, 200.0), Env { system_dark: false, scale: 1.0 });
    rt.frame(0.0);
    for (x, y, button) in [(50.0, 20.0, PointerButton::Back), (50.0, 150.0, PointerButton::Forward)] {
        rt.button_down(x, y, button);
        rt.button_up(x, y, button);
        rt.frame(0.0);
    }
    assert_eq!((rt.state.back, rt.state.forward, rt.state.row), (1, 1, 0), "over the row and over empty space");
    rt.button_down(250.0, 50.0, PointerButton::Back);
    rt.button_up(250.0, 50.0, PointerButton::Back);
    rt.frame(0.0);
    assert_eq!((rt.state.back, rt.state.canvas), (1, 1), "a deeper element taking the button keeps it");
}

#[test]
fn on_button_down_leaves_the_primary_button_alone() {
    let mut rt = Runtime::new(Buttons::default(), buttons, (300.0, 200.0), Env { system_dark: false, scale: 1.0 });
    rt.frame(0.0);
    rt.pointer_down(50.0, 150.0);
    rt.pointer_up(50.0, 150.0);
    rt.frame(0.0);
    assert_eq!((rt.state.back, rt.state.forward), (0, 0));
    assert!(rt.element_at(50.0, 150.0).is_some_and(|p| p.key() == Some("root")));
}
