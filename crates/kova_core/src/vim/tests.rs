//! Headless behaviour tests for the Vim engine.

use super::*;
use crate::keymap::{KeyCode, KeyStroke, Modifiers};
use kova_buffer::Buffer;

fn run(text: &str, keys: &str) -> VimEngine {
    let mut e = VimEngine::from_text(text);
    e.feed(keys);
    e
}

/// Runs `keys` from the cursor marked `|` in `start`; returns the text with `|` at the cursor.
fn ed(start: &str, keys: &str) -> String {
    let Some(pos) = start.find('|') else {
        panic!("cursor marker missing");
    };
    let text = start.replacen('|', "", 1);
    let mut e = VimEngine::from_text(&text);
    e.set_cursor(pos);
    e.feed(keys);
    let mut out = e.text();
    out.insert(e.cursor(), '|');
    out
}

const ESC: &str = "\u{1b}";

#[test]
fn modes_and_focus_context() {
    let mut e = VimEngine::from_text("abc");
    assert_eq!(e.mode(), VimMode::Normal);
    assert_eq!(e.status_snapshot().focus, FocusContext::EditorVimNormal);
    assert_eq!(e.cursor_shape(), VimCursorShape::Block);
    e.feed("i");
    assert_eq!(e.mode(), VimMode::Insert);
    assert_eq!(e.cursor_shape(), VimCursorShape::Bar);
    assert_eq!(e.status_snapshot().focus, FocusContext::EditorVimInsert);
    e.feed(ESC);
    e.feed("v");
    assert_eq!(e.status_snapshot().focus, FocusContext::EditorVimVisual);
    e.feed(ESC);
    e.feed("V");
    assert_eq!(e.status_snapshot().focus, FocusContext::EditorVimVisualLine);
    e.feed(ESC);
    e.feed("d");
    assert_eq!(e.cursor_shape(), VimCursorShape::Underline);
    assert_eq!(e.status_snapshot().pending, "d");
}

#[test]
fn pending_status_keys() {
    let mut e = VimEngine::from_text("abc def");
    e.feed("d2");
    assert_eq!(e.status_snapshot().pending, "d2");
    e.feed(ESC);
    assert_eq!(e.status_snapshot().pending, "");
    e.feed("g");
    assert_eq!(e.status_snapshot().pending, "g");
    e.feed(ESC);
    e.feed("ci");
    assert_eq!(e.status_snapshot().pending, "ci");
}

#[test]
fn insert_and_escape_moves_left() {
    assert_eq!(ed("|abc", "ixy\u{1b}"), "x|yabc");
    assert_eq!(ed("a|bc", "aXY\u{1b}"), "abX|Yc");
    assert_eq!(ed("  a|bc", "I-\u{1b}"), "  |-abc");
    assert_eq!(ed("a|bc", "A!\u{1b}"), "abc|!");
}

#[test]
fn open_lines() {
    assert_eq!(ed("a|\nb", "ox\u{1b}"), "a\n|x\nb");
    assert_eq!(ed("a\n|b", "Ox\u{1b}"), "a\n|x\nb");
    assert_eq!(ed("|a", "ox\u{1b}"), "a\n|x");
    assert_eq!(ed("|a", "3ox\u{1b}"), "a\nx\nx\n|x");
}

#[test]
fn insert_count_repeats() {
    assert_eq!(ed("|", "3ia\u{1b}"), "aa|a");
}

#[test]
fn insert_backspace_and_enter() {
    assert_eq!(ed("|", "iab\u{8}c\u{1b}"), "a|c");
    assert_eq!(ed("|", "ia\nb\u{1b}"), "a\n|b");
    assert_eq!(ed("a\n|b", "i\u{8}\u{1b}"), "|ab");
}

#[test]
fn basic_motions() {
    assert_eq!(ed("|abc def", "w"), "abc |def");
    assert_eq!(ed("abc |def", "b"), "|abc def");
    assert_eq!(ed("|abc def", "e"), "ab|c def");
    assert_eq!(ed("|abc def", "2w"), "abc de|f");
    assert_eq!(ed("abc d|ef", "ge"), "ab|c def");
    assert_eq!(ed("|a.b c", "W"), "a.b |c");
    assert_eq!(ed("|abc", "$"), "ab|c");
    assert_eq!(ed("ab|c", "0"), "|abc");
    assert_eq!(ed("|  abc", "$^"), "  |abc");
    assert_eq!(ed("a|bc", "l"), "ab|c");
    assert_eq!(ed("ab|c", "ll"), "ab|c");
    assert_eq!(ed("a|bc", "hh"), "|abc");
}

#[test]
fn vertical_motions_remember_column() {
    assert_eq!(ed("abc|d\nx\nabcd", "jj"), "abcd\nx\nabc|d");
    assert_eq!(ed("ab|cd\nx\nabcd", "j"), "abcd\n|x\nabcd");
    assert_eq!(ed("|a\nb\nc", "G"), "a\nb\n|c");
    assert_eq!(ed("a\nb\n|c", "gg"), "|a\nb\nc");
    assert_eq!(ed("|a\nb\nc", "2G"), "a\n|b\nc");
    assert_eq!(ed("|a\nb\nc", "3gg"), "a\nb\n|c");
    assert_eq!(ed("|abc\nd", "$j"), "abc\n|d");
}

#[test]
fn find_motions_and_repeat() {
    assert_eq!(ed("|a,b,c", "f,"), "a|,b,c");
    assert_eq!(ed("|a,b,c", "2f,"), "a,b|,c");
    assert_eq!(ed("|a,b,c", "t,"), "|a,b,c");
    assert_eq!(ed("|a,,b", "f,;"), "a,|,b");
    assert_eq!(ed("a,b,|c", "F,"), "a,b|,c");
    assert_eq!(ed("a,b,|c", "T,"), "a,b,|c");
    assert_eq!(ed("|a,b,c,d", "f,;,"), "a|,b,c,d");
    assert_eq!(ed("|ab,cd,ef", "t,;"), "ab,c|d,ef");
}

#[test]
fn paragraph_motions() {
    assert_eq!(ed("|a\nb\n\nc", "}"), "a\nb\n|\nc");
    assert_eq!(ed("a\nb\n\n|c", "{"), "a\nb\n|\nc");
    assert_eq!(ed("|a\nb", "}"), "a\n|b");
    assert_eq!(ed("a\nb|", "{"), "|a\nb");
}

#[test]
fn delete_with_motions() {
    assert_eq!(ed("|abc def", "dw"), "|def");
    assert_eq!(ed("|abc def ghi", "d2w"), "|ghi");
    assert_eq!(ed("abc |def", "dw"), "abc| ");
    assert_eq!(ed("|abc def", "de"), "| def");
    assert_eq!(ed("abc |def", "db"), "|def");
    assert_eq!(ed("|abcd", "d$"), "|");
    assert_eq!(ed("ab|cd", "d0"), "|cd");
    assert_eq!(ed("|a,b,c", "df,"), "|b,c");
    assert_eq!(ed("|a,b,c", "dt,"), "|,b,c");
    assert_eq!(ed("abc|d", "dh"), "ab|d");
    assert_eq!(ed("|abcd", "dl"), "|bcd");
}

#[test]
fn backward_motion_spans() {
    assert_eq!(ed("ab c|d", "dFb"), "a|d");
    assert_eq!(ed("abc|d", "dFb"), "a|d");
    assert_eq!(ed("abc|d", "dTb"), "ab|d");
    assert_eq!(ed("abc|d", "dFa"), "|d");
}

#[test]
fn dw_stops_at_end_of_line() {
    assert_eq!(ed("|foo\nbar", "dw"), "|\nbar");
    assert_eq!(ed("foo |bar\n baz", "dw"), "foo| \n baz");
    assert_eq!(ed("|foo\n\nbar", "dw"), "|\n\nbar");
}

#[test]
fn change_word_acts_like_ce() {
    assert_eq!(ed("|abc def", "cwX\u{1b}"), "|X def");
    assert_eq!(ed("ab|c def", "cwX\u{1b}"), "ab|X def");
    assert_eq!(ed("|abc def", "ceX\u{1b}"), "|X def");
    assert_eq!(ed("|abc def ghi", "c2wX\u{1b}"), "|X ghi");
    assert_eq!(ed("|a,b,c", "ct,X\u{1b}"), "|X,b,c");
    assert_eq!(ed("ab|c,d", "cFaX\u{1b}"), "|Xc,d");
}

#[test]
fn linewise_delete_and_cursor() {
    assert_eq!(ed("|a\nb\nc", "dd"), "|b\nc");
    assert_eq!(ed("a\n|b\nc", "dd"), "a\n|c");
    assert_eq!(ed("a\nb\n|c", "dd"), "a\n|b");
    assert_eq!(ed("|a\nb\nc", "2dd"), "|c");
    assert_eq!(ed("a\n|b\nc", "5dd"), "|a");
    assert_eq!(ed("|only", "dd"), "|");
    assert_eq!(ed("a\n|b\n", "dd"), "|a\n");
    assert_eq!(ed("  a\n|  b", "dd"), "  |a");
    assert_eq!(ed("|a\nb\nc", "dj"), "|c");
    assert_eq!(ed("a\n|b\nc", "dk"), "|c");
    assert_eq!(ed("|a\nb\nc", "dG"), "|");
    assert_eq!(ed("a\nb\n|c", "dgg"), "|");
}

#[test]
fn last_line_without_newline_removes_separator() {
    assert_eq!(ed("a\r\nb\r\n|c", "dd"), "a\r\n|b");
    assert_eq!(ed("a\nb\n|c", "dd"), "a\n|b");
}

#[test]
fn change_lines() {
    assert_eq!(ed("a\n|bc\nd", "ccX\u{1b}"), "a\n|X\nd");
    assert_eq!(ed("a\n|bc\nd", "SX\u{1b}"), "a\n|X\nd");
    assert_eq!(ed("|a\nb\nc", "2ccX\u{1b}"), "|X\nc");
}

#[test]
fn x_d_c_shortcuts() {
    assert_eq!(ed("a|bc", "x"), "a|c");
    assert_eq!(ed("ab|c", "x"), "a|b");
    assert_eq!(ed("|abcd", "2x"), "|cd");
    assert_eq!(ed("ab|c", "X"), "a|c");
    assert_eq!(ed("a|bc\nd", "D"), "|a\nd");
    assert_eq!(ed("a|bc", "CX\u{1b}"), "a|X");
    assert_eq!(ed("a|bc", "sX\u{1b}"), "a|Xc");
    assert_eq!(ed("|\nb", "sX\u{1b}"), "|X\nb");
    assert_eq!(ed("|", "x"), "|");
}

#[test]
fn yank_word_paste_before() {
    assert_eq!(ed("|abc def", "ywP"), "abc| abc def");
}

#[test]
fn yank_word_paste_after() {
    let e = run("foo bar", "yw$p");
    assert_eq!(e.text(), "foo barfoo ");
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("foo "));
}

#[test]
fn yy_paste_linewise() {
    assert_eq!(ed("|a\nb", "yyp"), "a\n|a\nb");
    assert_eq!(ed("a\n|b", "yyp"), "a\nb\n|b");
    assert_eq!(ed("a\n|b", "yyP"), "a\n|b\nb");
    assert_eq!(ed("|a\nb", "yy2p"), "a\n|a\na\nb");
    assert_eq!(ed("|a\nb", "ddp"), "b\n|a");
    assert_eq!(ed("|a\nb\nc", "yjGp"), "a\nb\nc\n|a\nb");
}

#[test]
fn charwise_paste_variants() {
    assert_eq!(ed("|ab", "xp"), "b|a");
    assert_eq!(ed("a|b", "xP"), "|ba");
    assert_eq!(ed("|abc", "ylp"), "a|abc");
    assert_eq!(ed("|ab", "yl3p"), "aaa|ab");
}

#[test]
fn named_registers_and_blackhole() {
    let mut e = VimEngine::from_text("one two");
    e.feed("\"ayw");
    e.feed("w\"_dw");
    assert_eq!(e.text(), "one ");
    assert_eq!(e.register('a').map(|r| r.text.as_str()), Some("one "));
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("one "));
    e.feed("\"aP");
    assert_eq!(e.text(), "oneone  ");
}

#[test]
fn indent_and_outdent() {
    assert_eq!(ed("|a\nb", ">>"), "    |a\nb");
    assert_eq!(ed("|a\nb\nc", "2>>"), "    |a\n    b\nc");
    assert_eq!(ed("|a\nb", ">j"), "    |a\n    b");
    assert_eq!(ed("|    a", "<<"), "|a");
    assert_eq!(ed("|  a", "<<"), "|a");
    assert_eq!(ed("|\ta", "<<"), "|a");
    assert_eq!(ed("|a\n\nb", ">}"), "    |a\n\nb");
    assert_eq!(ed("|a\n\nb", ">G"), "    |a\n\n    b");
}

#[test]
fn case_operators() {
    assert_eq!(ed("|abc def", "gUw"), "|ABC def");
    assert_eq!(ed("|ABC", "guu"), "|abc");
    assert_eq!(ed("|abc", "gUU"), "|ABC");
    assert_eq!(ed("|abc", "gUgU"), "|ABC");
    assert_eq!(ed("|aBc", "g~~"), "|AbC");
    assert_eq!(ed("|aB", "~"), "A|B");
    assert_eq!(ed("|abc", "2~"), "AB|c");
    assert_eq!(ed("|zaz", "gUl"), "|Zaz");
    assert_eq!(ed("|zażółć", "gUU"), "|ZAŻÓŁĆ");
}

#[test]
fn text_objects_word() {
    assert_eq!(ed("foo b|ar baz", "diw"), "foo | baz");
    assert_eq!(ed("foo b|ar baz", "daw"), "foo |baz");
    assert_eq!(ed("foo |bar", "daw"), "fo|o");
    assert_eq!(ed("foo b|ar baz", "ciwX\u{1b}"), "foo |X baz");
    assert_eq!(ed("foo b|ar baz", "yiwP"), "foo ba|rbar baz");
    assert_eq!(ed("a|  b", "diw"), "a|b");
    assert_eq!(ed("foo.b|ar", "diw"), "foo|.");
    assert_eq!(ed("a.b |c", "diW"), "a.b| ");
}

#[test]
fn text_objects_quotes() {
    assert_eq!(ed("x \"h|i\" y", "di\""), "x \"|\" y");
    assert_eq!(ed("x \"h|i\" y", "da\""), "x |y");
    assert_eq!(ed("x |a \"hi\" y", "di\""), "x a \"|\" y");
    assert_eq!(ed("x \"h|i\"", "da\""), "|x");
    assert_eq!(ed("x \"h|i\" y", "ci\"Z\u{1b}"), "x \"|Z\" y");
    assert_eq!(ed("'a| b'", "di'"), "'|'");
    assert_eq!(ed("x \"a\\\"b|\" y", "di\""), "x \"|\" y");
    assert_eq!(ed("abc|", "di\""), "ab|c");
}

#[test]
fn text_objects_brackets() {
    assert_eq!(ed("f(a, |b)", "di("), "f(|)");
    assert_eq!(ed("f(a, |b)", "da("), "|f");
    assert_eq!(ed("f(a, |b)", "dib"), "f(|)");
    assert_eq!(ed("f(a(|b)c)", "di("), "f(a(|)c)");
    assert_eq!(ed("f(a(b)|c)", "di("), "f(|)");
    assert_eq!(ed("|f(ab)", "di("), "|f(ab)");
    assert_eq!(ed("[a|b]", "di["), "[|]");
    assert_eq!(ed("<a|b>", "da<"), "|");
    assert_eq!(ed("x {a|b} y", "ci{Z\u{1b}"), "x {|Z} y");
    assert_eq!(ed("f {\n  x|\n}", "di{"), "f {\n|}");
    assert_eq!(ed("f {\n  x|\n}", "da{"), "f| ");
    assert_eq!(ed("f {\n  x|\n}", "diB"), "f {\n|}");
    assert_eq!(ed("abc|", "di("), "ab|c");
}

#[test]
fn visual_charwise_ops() {
    assert_eq!(ed("|abcd", "vld"), "|cd");
    assert_eq!(ed("|abcd", "vly"), "|abcd");
    assert_eq!(ed("|abcd", "vlyP"), "a|babcd");
    assert_eq!(ed("ab|cd", "vhd"), "a|d");
    assert_eq!(ed("|ab cd", "vwd"), "|d");
    assert_eq!(ed("|abc", "v$d"), "|");
    assert_eq!(ed("|abc", "vlc-\u{1b}"), "|-c");
    assert_eq!(ed("|abc", "vlx"), "|c");
    assert_eq!(ed("|abc", "vlU"), "|ABc");
    assert_eq!(ed("|aBc", "vll~"), "|AbC");
    assert_eq!(ed("|ABc", "vlu"), "|abc");
    assert_eq!(ed("|abc", "vlrz"), "|zzc");
    assert_eq!(ed("|a\n b", "vjJ"), "a| b");
    assert_eq!(ed("|abcd", "vl>"), "    |abcd");
}

#[test]
fn visual_object_and_swap() {
    assert_eq!(ed("x f|oo y", "viwd"), "x | y");
    assert_eq!(ed("f(a|b)", "vi(d"), "f(|)");
    let mut e = run("abcd", "vl");
    assert_eq!(e.selection(), Some(0..2));
    e.feed("o");
    assert_eq!(e.cursor(), 0);
    assert_eq!(e.selection(), Some(0..2));
    e.feed(ESC);
    assert_eq!(e.selection(), None);
    assert_eq!(e.mode(), VimMode::Normal);
}

#[test]
fn visual_line_ops() {
    assert_eq!(ed("|a\nb\nc", "Vjd"), "|c");
    assert_eq!(ed("a\n|b\nc", "Vd"), "a\n|c");
    assert_eq!(ed("a\nb\n|c", "Vd"), "a\n|b");
    assert_eq!(ed("|a\nb", "Vyp"), "a\n|a\nb");
    assert_eq!(ed("|a\nb\nc", "Vj>"), "    |a\n    b\nc");
    assert_eq!(ed("|a\nb", "VcX\u{1b}"), "|X\nb");
    assert_eq!(ed("|ab", "VU"), "|AB");
    let e = run("a\nb\nc", "Vj");
    assert_eq!(e.selection(), Some(0..4));
}

#[test]
fn visual_mode_switching_and_gv() {
    let mut e = run("abc\ndef", "vl");
    e.feed("V");
    assert_eq!(e.mode(), VimMode::VisualLine);
    e.feed("v");
    assert_eq!(e.mode(), VimMode::Visual);
    e.feed("v");
    assert_eq!(e.mode(), VimMode::Normal);
    e.feed("gv");
    assert_eq!(e.mode(), VimMode::Visual);
    assert_eq!(e.selection(), Some(0..2));
    e.feed(ESC);
    e.feed("jV");
    e.feed("V");
    assert_eq!(e.mode(), VimMode::Normal);
}

#[test]
fn visual_paste_replaces_selection() {
    assert_eq!(ed("|ab cd", "yiwwviwp"), "ab a|b");
    let e = run("ab cd", "yiwwviwp");
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("cd"));
}

#[test]
fn replace_command() {
    assert_eq!(ed("|abc", "rx"), "|xbc");
    assert_eq!(ed("|abc", "2rx"), "x|xc");
    assert_eq!(ed("|abc", "5rx"), "|abc");
    assert_eq!(ed("a|bc", "r\n"), "a\n|c");
    assert_eq!(ed("|abc", "rż"), "|żbc");
}

#[test]
fn join_lines() {
    assert_eq!(ed("|a\n  b", "J"), "a| b");
    assert_eq!(ed("|a\nb\nc", "3J"), "a b| c");
    assert_eq!(ed("|a\n)", "J"), "a|)");
    assert_eq!(ed("|a\nb", "gJ"), "a|b");
    assert_eq!(ed("|a", "J"), "|a");
    assert_eq!(ed("|a\r\n  b", "J"), "a| b");
}

#[test]
fn undo_redo() {
    let mut e = run("abc def", "dwdw");
    assert_eq!(e.text(), "");
    e.feed("u");
    assert_eq!(e.text(), "def");
    e.feed("u");
    assert_eq!(e.text(), "abc def");
    e.feed("\u{12}");
    assert_eq!(e.text(), "def");
    e.feed("\u{12}");
    assert_eq!(e.text(), "");
    e.feed("\u{12}");
    assert_eq!(e.text(), "");
    e.feed("2u");
    assert_eq!(e.text(), "abc def");
}

#[test]
fn insert_session_is_one_undo_step() {
    let mut e = run("", "ihello world\u{1b}");
    assert_eq!(e.text(), "hello world");
    e.feed("u");
    assert_eq!(e.text(), "");
    e.feed("\u{12}");
    assert_eq!(e.text(), "hello world");
}

#[test]
fn new_edit_clears_redo() {
    let mut e = run("abc", "xu");
    assert_eq!(e.text(), "abc");
    e.feed("x\u{12}");
    assert_eq!(e.text(), "bc");
}

#[test]
fn dot_repeat() {
    assert_eq!(ed("|a b c d", "dw."), "|c d");
    assert_eq!(ed("|ab ab", "cwX\u{1b}w."), "X |X");
    assert_eq!(ed("|a\nb\nc", "dd."), "|c");
    assert_eq!(ed("|x", "ia\u{1b}."), "|aax");
    assert_eq!(ed("|abc", "x."), "|c");
    assert_eq!(ed("|a b", "Ax\u{1b}."), "a bx|x");
    assert_eq!(ed("|a\nb", "ox\u{1b}."), "a\nx\n|x\nb");
    assert_eq!(ed("|a b", "yw."), "|a b");
    let e = run("a b c d", "dw.u");
    assert_eq!(e.text(), "b c d");
}

#[test]
fn counts_multiply() {
    assert_eq!(ed("|a b c d e f", "2d2w"), "|e f");
    assert_eq!(ed("|a\nb\nc\nd", "2d2j"), "|");
    assert_eq!(ed("|a b", "0dw"), "|b");
    assert_eq!(
        ed("|a b c d e f g h i j k l", "10w"),
        "a b c d e f g h i j |k l"
    );
}

#[test]
fn escape_cancels_pending() {
    assert_eq!(ed("|abc", "d\u{1b}x"), "|bc");
    assert_eq!(ed("|abc", "2\u{1b}x"), "|bc");
    assert_eq!(ed("|abc", "df\u{1b}x"), "|bc");
    assert_eq!(ed("|abc", "dix"), "|abc");
}

#[test]
fn invalid_motions_do_nothing() {
    assert_eq!(ed("|abc", "dh"), "|abc");
    assert_eq!(ed("a|bc", "dk"), "a|bc");
    assert_eq!(ed("a|bc", "dj"), "a|bc");
    assert_eq!(ed("|abc", "dfz"), "|abc");
    assert_eq!(ed("|abc", "dib"), "|abc");
}

#[test]
fn utf8_polish_and_emoji() {
    assert_eq!(ed("|zażółć gęślą", "w"), "zażółć |gęślą");
    assert_eq!(ed("|zażółć", "$"), "zażół|ć");
    assert_eq!(ed("|zażółć", "x"), "|ażółć");
    assert_eq!(ed("|zażółć", "3l"), "zaż|ółć");
    assert_eq!(ed("zażół|ć", "dh"), "zażó|ć");
    assert_eq!(ed("|ąę ść", "dw"), "|ść");
    assert_eq!(ed("|a😀b", "ll"), "a😀|b");
    assert_eq!(ed("a|😀b", "x"), "a|b");
    assert_eq!(ed("a😀|b", "dh"), "a|b");
    assert_eq!(ed("|a😀b", "f😀"), "a|😀b");
    assert_eq!(ed("|ab", "i😀\u{1b}"), "|😀ab");
    assert_eq!(ed("|zażółć", "~"), "Z|ażółć");
    assert_eq!(ed("|ą", "rę"), "|ę");
    assert_eq!(ed("ż|ó", "diw"), "|");
}

#[test]
fn crlf_is_atomic() {
    assert_eq!(ed("|ab\r\ncd", "$"), "a|b\r\ncd");
    assert_eq!(ed("|ab\r\ncd", "j"), "ab\r\n|cd");
    assert_eq!(ed("|ab\r\ncd", "dd"), "|cd");
    assert_eq!(ed("|ab\r\ncd", "ox\u{1b}"), "ab\r\n|x\r\ncd");
    assert_eq!(ed("|ab\r\ncd", "Ox\u{1b}"), "|x\r\nab\r\ncd");
    assert_eq!(ed("ab\r\n|cd", "i\u{8}\u{1b}"), "a|bcd");
    assert_eq!(ed("|ab\r\ncd", "yyp"), "ab\r\n|ab\r\ncd");
    assert_eq!(ed("|ab\r\ncd", "$x"), "|a\r\ncd");
    assert_eq!(ed("|ab\r\ncd", "Vjd"), "|");
    assert_eq!(ed("|ab\r\n\r\ncd", "}"), "ab\r\n|\r\ncd");
    assert_eq!(ed("|a b\r\nc", "dw"), "|b\r\nc");
    assert_eq!(ed("|ab\r\ncd", "ia\nb\u{1b}"), "a\r\n|bab\r\ncd");
    let e = run("a\r\nb", "dd");
    assert!(e.last_error().is_none());
    assert_eq!(e.buffer().snapshot().text(), "b");
}

#[test]
fn buffer_stays_in_sync() {
    let mut e = run("one two", "dwiX\u{1b}");
    assert_eq!(e.buffer().snapshot().text(), e.text());
    e.feed("u");
    assert_eq!(e.buffer().snapshot().text(), e.text());
    let b = e.into_buffer();
    assert_eq!(b.snapshot().text(), "two");
    let e2 = VimEngine::new(Buffer::from_text("zz"));
    assert_eq!(e2.cursor_point().map(|p| (p.row, p.column)), Some((0, 0)));
}

fn stroke(c: char, mods: Modifiers) -> KeyStroke {
    KeyStroke::new(mods, KeyCode::Char(c))
}

#[test]
fn keystroke_decoding_of_shifted_letters() {
    let mut e = VimEngine::from_text("a\nb\nc");
    assert_eq!(
        e.handle_key(stroke('g', Modifiers::SHIFT)),
        VimOutcome::Handled
    );
    assert_eq!(e.cursor(), 4);
    e.handle_key(stroke('d', Modifiers::NONE));
    e.handle_key(stroke('d', Modifiers::NONE));
    assert_eq!(e.text(), "a\nb");
    let mut e = VimEngine::from_text("abc def");
    e.handle_key(stroke('d', Modifiers::SHIFT));
    assert_eq!(e.text(), "");
    let mut e = VimEngine::from_text("a\nb");
    e.handle_key(stroke('j', Modifiers::SHIFT));
    assert_eq!(e.text(), "a b");
}

#[test]
fn escape_and_ctrl_keys() {
    let mut e = VimEngine::from_text("abc");
    e.handle_key(stroke('i', Modifiers::NONE));
    e.handle_key(stroke('x', Modifiers::NONE));
    assert_eq!(
        e.handle_key(KeyStroke::unmodified(KeyCode::Escape)),
        VimOutcome::Handled
    );
    assert_eq!(e.mode(), VimMode::Normal);
    e.handle_key(stroke('i', Modifiers::NONE));
    e.handle_key(stroke('c', Modifiers::CTRL));
    assert_eq!(e.mode(), VimMode::Normal);
    e.handle_key(stroke('i', Modifiers::NONE));
    e.handle_key(stroke('[', Modifiers::CTRL));
    assert_eq!(e.mode(), VimMode::Normal);
    e.handle_key(stroke('u', Modifiers::NONE));
    e.handle_key(stroke('u', Modifiers::NONE));
    assert_eq!(e.text(), "abc");
    assert_eq!(
        e.handle_key(stroke('r', Modifiers::CTRL)),
        VimOutcome::Handled
    );
    assert_eq!(e.text(), "xabc");
}

#[test]
fn ide_shortcuts_pass_through() {
    let mut e = VimEngine::from_text("abc");
    let alt = Modifiers {
        alt: true,
        ..Modifiers::NONE
    };
    let meta = Modifiers {
        meta: true,
        ..Modifiers::NONE
    };
    assert_eq!(
        e.handle_key(KeyStroke::new(alt, KeyCode::Enter)),
        VimOutcome::PassThrough
    );
    assert_eq!(e.handle_key(stroke('/', meta)), VimOutcome::PassThrough);
    assert_eq!(
        e.handle_key(KeyStroke::unmodified(KeyCode::F(6))),
        VimOutcome::PassThrough
    );
    assert_eq!(
        e.handle_key(stroke('e', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(e.text(), "abc");
    assert_eq!(e.mode(), VimMode::Normal);
}

#[test]
fn conflict_policy_routes_ctrl_chords() {
    let mut e = VimEngine::from_text("a\nb\nc");
    assert_eq!(
        e.handle_key(stroke('d', Modifiers::CTRL)),
        VimOutcome::Handled
    );
    assert_eq!(e.cursor(), 4);
    e.set_ctrl_policy('d', KeyConflictPolicy::Ide);
    e.set_cursor(0);
    assert_eq!(
        e.handle_key(stroke('d', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(e.cursor(), 0);
    e.set_default_policy(KeyConflictPolicy::Ide);
    assert_eq!(
        e.handle_key(stroke('r', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(
        e.handle_key(stroke('u', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(
        e.handle_key(stroke('c', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    e.set_ctrl_policy('r', KeyConflictPolicy::Vim);
    assert_eq!(
        e.handle_key(stroke('r', Modifiers::CTRL)),
        VimOutcome::Handled
    );
}

#[test]
fn insert_mode_passes_unowned_ctrl() {
    let mut e = VimEngine::from_text("a");
    e.feed("i");
    assert_eq!(
        e.handle_key(stroke('v', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(
        e.handle_key(stroke('r', Modifiers::CTRL)),
        VimOutcome::PassThrough
    );
    assert_eq!(e.mode(), VimMode::Insert);
}

#[test]
fn page_and_scroll_motions() {
    let text = (0..50)
        .map(|i| format!("l{i}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut e = VimEngine::from_text(&text);
    e.feed("\u{4}");
    assert_eq!(e.cursor_point().map(|p| p.row), Some(10));
    e.feed("\u{15}");
    assert_eq!(e.cursor_point().map(|p| p.row), Some(0));
    e.feed("\u{6}");
    assert_eq!(e.cursor_point().map(|p| p.row), Some(20));
    e.feed("\u{2}");
    assert_eq!(e.cursor_point().map(|p| p.row), Some(0));
}

#[test]
fn arrow_keys_work_like_hjkl() {
    let mut e = VimEngine::from_text("ab\ncd");
    e.handle_key(KeyStroke::unmodified(KeyCode::Right));
    assert_eq!(e.cursor(), 1);
    e.handle_key(KeyStroke::unmodified(KeyCode::Down));
    assert_eq!(e.cursor(), 4);
    e.handle_key(KeyStroke::unmodified(KeyCode::Left));
    assert_eq!(e.cursor(), 3);
    e.handle_key(KeyStroke::unmodified(KeyCode::Up));
    assert_eq!(e.cursor(), 0);
    e.handle_key(KeyStroke::unmodified(KeyCode::End));
    assert_eq!(e.cursor(), 1);
    e.handle_key(KeyStroke::unmodified(KeyCode::Home));
    assert_eq!(e.cursor(), 0);
}

#[test]
fn insert_mode_arrows_and_delete() {
    let mut e = VimEngine::from_text("abc");
    e.feed("A");
    e.handle_key(KeyStroke::unmodified(KeyCode::Left));
    e.handle_key(KeyStroke::unmodified(KeyCode::Delete));
    e.feed(ESC);
    assert_eq!(e.text(), "ab");
    let mut e = VimEngine::from_text("ab\ncd");
    e.feed("i");
    e.handle_key(KeyStroke::unmodified(KeyCode::Down));
    e.feed("X");
    assert_eq!(e.text(), "ab\nXcd");
    e.handle_key(KeyStroke::unmodified(KeyCode::Up));
    e.feed("Y");
    assert_eq!(e.text(), "aYb\nXcd");
}

#[test]
fn ctrl_w_deletes_word_in_insert() {
    let mut e = VimEngine::from_text("");
    e.feed("ifoo bar\u{17}x\u{1b}");
    assert_eq!(e.text(), "foo x");
}

#[test]
fn set_cursor_clamps_to_boundary_and_mode() {
    let mut e = VimEngine::from_text("żab");
    e.set_cursor(1);
    assert_eq!(e.cursor(), 0);
    e.set_cursor(99);
    assert_eq!(e.cursor(), 3);
    e.feed("i");
    e.set_cursor(99);
    assert_eq!(e.cursor(), 4);
}

#[test]
fn empty_buffer_is_safe() {
    let mut e = VimEngine::from_text("");
    e.feed("xdwddpPJrxwbe$0GgG{}ggu~yyvd");
    e.feed(ESC);
    assert!(e.cursor() <= e.text().len());
    assert!(e.last_error().is_none());
}

#[test]
fn from_char_decodes_scripts() {
    assert_eq!(VimKey::from_char('\u{1b}'), VimKey::Escape);
    assert_eq!(VimKey::from_char('\u{12}'), VimKey::Ctrl('r'));
    assert_eq!(VimKey::from_char('\n'), VimKey::Enter);
    assert_eq!(VimKey::from_char('x'), VimKey::Char('x'));
}

#[test]
fn performance_smoke_large_buffer() {
    let text = "let x = foo(bar, baz);\n".repeat(100_000);
    let mut e = VimEngine::from_text(&text);
    let start = std::time::Instant::now();
    e.feed("Gddggwdwju.");
    assert!(start.elapsed() < std::time::Duration::from_millis(250));
}

#[test]
fn percent_bracket_matching() {
    assert_eq!(ed("|foo(a(b)c)", "%"), "foo(a(b)c|)");
    assert_eq!(ed("foo(a(b)c|)", "%"), "foo|(a(b)c)");
    assert_eq!(ed("foo|(a(b)c)", "d%"), "fo|o");
    assert_eq!(ed("foo|(a(b)c)", "c%X\u{1b}"), "foo|X");
    assert_eq!(ed("{\n  [x]\n|}", "%"), "|{\n  [x]\n}");
    assert_eq!(ed("foo|(a(b)c)", "v%d"), "fo|o");
    assert_eq!(ed("|abc", "%"), "|abc");
}

#[test]
fn registers_numbered_append_and_clipboard() {
    let mut e = VimEngine::from_text("first\nsecond\nthird");
    e.feed("yy");
    assert_eq!(e.register('0').map(|r| r.text.as_str()), Some("first\n"));
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("first\n"));

    e.feed("jdd");
    // `dd` shifts into "1 without overwriting "0.
    assert_eq!(e.register('0').map(|r| r.text.as_str()), Some("first\n"));
    assert_eq!(e.register('1').map(|r| r.text.as_str()), Some("second\n"));
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("second\n"));

    e.feed("dd");
    assert_eq!(e.register('1').map(|r| r.text.as_str()), Some("third\n"));
    assert_eq!(e.register('2').map(|r| r.text.as_str()), Some("second\n"));
    assert_eq!(e.register('0').map(|r| r.text.as_str()), Some("first\n"));

    // Uppercase register appends to lowercase register.
    let mut e = VimEngine::from_text("alpha beta gamma");
    e.feed("\"adw\"Adw");
    assert_eq!(
        e.register('a').map(|r| r.text.as_str()),
        Some("alpha beta ")
    );
    assert_eq!(
        e.register('"').map(|r| r.text.as_str()),
        Some("alpha beta ")
    );

    // `+` and `*` clipboard registers.
    let mut e = VimEngine::from_text("hello");
    e.feed("\"+yy\"+p");
    assert_eq!(e.register('+').map(|r| r.text.as_str()), Some("hello\n"));
    assert_eq!(e.text(), "hello\nhello");
    e.feed("\"*yy\"*p");
    assert_eq!(e.register('*').map(|r| r.text.as_str()), Some("hello\n"));
    assert_eq!(e.text(), "hello\nhello\nhello");
}

#[test]
fn count_in_bracket_text_objects() {
    assert_eq!(ed("{outer {in|ner} end}", "2di{"), "{|}");
    assert_eq!(ed("f(a(b|c)d)", "2ci(Z\u{1b}"), "f(|Z)");
    assert_eq!(ed("[[x|y]]", "2da["), "|");
}

#[test]
fn handle_text_input_ime_and_await() {
    let mut e = VimEngine::from_text("bąk");
    // Normal mode not awaiting -> PassThrough.
    assert_eq!(e.handle_text_input("ą"), VimOutcome::PassThrough);
    // `f` + IME `ą`.
    e.feed("f");
    assert_eq!(e.handle_text_input("ą"), VimOutcome::Handled);
    assert_eq!(e.cursor(), 1);
    // `r` + IME `ż`.
    e.feed("r");
    assert_eq!(e.handle_text_input("ż"), VimOutcome::Handled);
    assert_eq!(e.text(), "bżk");
    // Insert mode multi-byte IME string + dot-repeat.
    e.feed("A");
    assert_eq!(e.handle_text_input(" żółć🚀"), VimOutcome::Handled);
    e.feed(ESC);
    assert_eq!(e.text(), "bżk żółć🚀");
}

#[test]
fn replace_buffer_keeps_mode_and_registers_and_clamps_cursor() {
    let mut e = VimEngine::from_text("alpha beta gamma");
    e.feed("wyw$");
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("beta "));
    let before_mode = e.mode();

    // A sibling split shortened the document behind the engine's back.
    e.replace_buffer(Buffer::from_text("abc"));
    assert_eq!(e.mode(), before_mode);
    assert_eq!(e.text(), "abc");
    assert_eq!(
        e.cursor(),
        2,
        "cursor clamps onto the last char in Normal mode"
    );
    assert_eq!(e.register('"').map(|r| r.text.as_str()), Some("beta "));

    // Engine undo history referred to the old text and is dropped: `u` is a no-op.
    e.feed("u");
    assert_eq!(e.text(), "abc");

    // Editing continues normally on the new buffer.
    e.feed("0P");
    assert_eq!(e.text(), "beta abc");

    // In Insert mode the cursor may sit at the end of the text.
    e.feed("A");
    e.replace_buffer(Buffer::from_text("xy"));
    assert_eq!(e.mode(), VimMode::Insert);
    assert_eq!(e.cursor(), 2);
    assert_eq!(e.handle_text_input("ż"), VimOutcome::Handled);
    e.feed(ESC);
    assert_eq!(e.text(), "xyż");
}

#[test]
fn gd_triggers_go_to_definition_only_in_idle_normal_mode() {
    let mut e = VimEngine::from_text("OrderService service;");
    e.set_cursor(15);

    assert_eq!(e.handle_vim_key(VimKey::Char('g')), VimOutcome::Handled);
    assert_eq!(e.status_snapshot().pending, "g");
    assert_eq!(
        e.handle_vim_key(VimKey::Char('d')),
        VimOutcome::GoToDefinition
    );
    assert_eq!(e.status_snapshot().pending, "");
    assert_eq!(e.text(), "OrderService service;");
    assert_eq!(e.cursor(), 15);

    // Operator-pending `dgd` must not trigger GoToDefinition.
    assert_eq!(e.handle_vim_key(VimKey::Char('d')), VimOutcome::Handled);
    assert_eq!(e.handle_vim_key(VimKey::Char('g')), VimOutcome::Handled);
    assert_eq!(e.handle_vim_key(VimKey::Char('d')), VimOutcome::Handled);
    assert_eq!(e.text(), "OrderService service;");

    // Visual mode `vgd` must not trigger GoToDefinition.
    assert_eq!(e.handle_vim_key(VimKey::Char('v')), VimOutcome::Handled);
    assert_eq!(e.handle_vim_key(VimKey::Char('g')), VimOutcome::Handled);
    assert_eq!(e.handle_vim_key(VimKey::Char('d')), VimOutcome::Handled);
}
