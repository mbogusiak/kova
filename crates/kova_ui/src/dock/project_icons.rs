//! Crisp GPUI vector icons (`canvas` + `PathBuilder` + `paint_quad`) for the `Project` dock
//! and its header toolbar (`KOVA-055`).
//!
//! All icons are drawn resolution-independently on `16×16` (or `14×16` / `10×10`) logical grids
//! without external asset files or startup I/O.

use gpui::{
    AnyElement, BorderStyle, Bounds, Hsla, IntoElement, PathBuilder, Pixels, Point, Styled, canvas,
    fill, outline, point, px, size,
};
use kova_theme::ThemePolarity;

use crate::dock::project_tree::FileIconKind;
use crate::theme::{KovaTheme, hsla_from_rgba};

/// Resolved icon colors for a single frame based on the active [`KovaTheme`] polarity.
#[derive(Debug, Clone, Copy)]
pub struct ProjectIconPalette {
    /// Theme island background (used to cut a halo around the root module badge).
    pub island_bg: Hsla,
    /// Outlined folder stroke color.
    pub folder_stroke: Hsla,
    /// Blue module badge fill on the project root folder.
    pub module_badge: Hsla,
    /// Warm amber tint applied to both icon and text of `git-ignored` nodes.
    pub git_ignored: Hsla,
    /// Markdown `M↓` icon color.
    pub markdown: Hsla,
    /// Java icon color.
    pub java: Hsla,
    /// Java test file icon color.
    pub java_test: Hsla,
    /// Kotlin primary ribbon color.
    pub kotlin_primary: Hsla,
    /// Kotlin secondary ribbon color.
    pub kotlin_accent: Hsla,
    /// YAML icon color.
    pub yaml: Hsla,
    /// XML / POM icon color.
    pub xml: Hsla,
    /// Groovy / Gradle icon color.
    pub groovy: Hsla,
    /// Rust icon color.
    pub rust: Hsla,
    /// JSON icon color.
    pub json: Hsla,
    /// TOML icon color.
    pub toml: Hsla,
    /// `kova.toml` highlight icon color.
    pub kova_config: Hsla,
    /// HTTP request primary icon color.
    pub http_primary: Hsla,
    /// HTTP request accent color.
    pub http_accent: Hsla,
    /// Shell script icon color.
    pub shell: Hsla,
    /// Default/generic file and dotfile color.
    pub default_file: Hsla,
}

impl ProjectIconPalette {
    /// Builds a [`ProjectIconPalette`] from the active [`KovaTheme`].
    #[must_use]
    pub fn from_theme(theme: &KovaTheme) -> Self {
        let is_dark = theme.active.metadata.polarity == ThemePolarity::Dark;
        if is_dark {
            Self {
                island_bg: hsla_from_rgba(theme.surfaces().island),
                folder_stroke: hex(0x009A_A4B6),
                module_badge: hex(0x0035_74F0),
                git_ignored: hex(0x00D1_9A66),
                markdown: hex(0x0054_8AF7),
                java: hex(0x00E0_6C75),
                java_test: hex(0x004A_DE80),
                kotlin_primary: hex(0x008B_5CF6),
                kotlin_accent: hex(0x00D9_46EF),
                yaml: hex(0x00F8_7171),
                xml: hex(0x00E5_A04D),
                groovy: hex(0x0056_D4DD),
                rust: hex(0x00DE_7A48),
                json: hex(0x00F5_C451),
                toml: hex(0x007D_D3FC),
                kova_config: hex(0x00FB_BF24),
                http_primary: hex(0x00A7_8BFA),
                http_accent: hex(0x0034_D399),
                shell: hex(0x004A_DE80),
                default_file: hsla_from_rgba(theme.text().secondary),
            }
        } else {
            Self {
                island_bg: hsla_from_rgba(theme.surfaces().island),
                folder_stroke: hsla_from_rgba(theme.text().secondary),
                module_badge: hex(0x0025_63EB),
                git_ignored: hex(0x009A_6010),
                markdown: hex(0x0025_63EB),
                java: hex(0x00DC_2626),
                java_test: hex(0x0016_A34A),
                kotlin_primary: hex(0x007C_3AED),
                kotlin_accent: hex(0x00C0_26D3),
                yaml: hex(0x00DC_2626),
                xml: hex(0x00C2_410C),
                groovy: hex(0x000E_7490),
                rust: hex(0x00C2_410C),
                json: hex(0x00B4_5309),
                toml: hex(0x0002_84C7),
                kova_config: hex(0x00B4_5309),
                http_primary: hex(0x007C_3AED),
                http_accent: hex(0x0005_9669),
                shell: hex(0x0015_803D),
                default_file: hsla_from_rgba(theme.text().secondary),
            }
        }
    }
}

fn hex(rgb: u32) -> Hsla {
    let [_, r, g, b] = rgb.to_be_bytes();
    gpui::Rgba {
        r: f32::from(r) / 255.0,
        g: f32::from(g) / 255.0,
        b: f32::from(b) / 255.0,
        a: 1.0,
    }
    .into()
}

pub(crate) fn vector_icon(
    width: f32,
    height: f32,
    paint: impl Fn(f32, f32, &mut gpui::Window) + 'static,
) -> AnyElement {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _cx| {
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);
            paint(ox, oy, window);
        },
    )
    .w(px(width))
    .h(px(height))
    .flex_none()
    .into_any_element()
}

pub(crate) fn pt(ox: f32, oy: f32, x: f32, y: f32) -> Point<Pixels> {
    point(px(ox + x), px(oy + y))
}

pub(crate) fn stroke_segments(
    window: &mut gpui::Window,
    width: f32,
    color: Hsla,
    segments: &[&[(f32, f32)]],
    ox: f32,
    oy: f32,
) {
    let mut builder = PathBuilder::stroke(px(width));
    for seg in segments {
        if let Some((first, rest)) = seg.split_first() {
            builder.move_to(pt(ox, oy, first.0, first.1));
            for &(x, y) in rest {
                builder.line_to(pt(ox, oy, x, y));
            }
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

pub(crate) fn fill_polygon(
    window: &mut gpui::Window,
    color: Hsla,
    pts: &[(f32, f32)],
    ox: f32,
    oy: f32,
) {
    let Some((first, rest)) = pts.split_first() else {
        return;
    };
    let mut builder = PathBuilder::fill();
    builder.move_to(pt(ox, oy, first.0, first.1));
    for &(x, y) in rest {
        builder.line_to(pt(ox, oy, x, y));
    }
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

pub(crate) fn paint_rounded_rect(
    window: &mut gpui::Window,
    origin: (f32, f32),
    rect: (f32, f32, f32, f32),
    radius: f32,
    color: Hsla,
) {
    let (ox, oy) = origin;
    let (x, y, w, h) = rect;
    let mut quad = fill(Bounds::new(pt(ox, oy, x, y), size(px(w), px(h))), color);
    quad.corner_radii = px(radius).into();
    window.paint_quad(quad);
}

pub(crate) fn paint_outlined_rounded_rect(
    window: &mut gpui::Window,
    origin: (f32, f32),
    rect: (f32, f32, f32, f32),
    radius: f32,
    stroke_w: f32,
    color: Hsla,
) {
    let (ox, oy) = origin;
    let (x, y, w, h) = rect;
    let mut quad = outline(
        Bounds::new(pt(ox, oy, x, y), size(px(w), px(h))),
        color,
        BorderStyle::Solid,
    );
    quad.corner_radii = px(radius).into();
    quad.border_widths = gpui::Edges::all(px(stroke_w));
    window.paint_quad(quad);
}

pub(crate) fn paint_folder_outline(
    ox: f32,
    oy: f32,
    window: &mut gpui::Window,
    stroke_width: f32,
    color: Hsla,
) {
    let mut builder = PathBuilder::stroke(px(stroke_width));
    builder.move_to(pt(ox, oy, 2.0, 4.2));
    builder.line_to(pt(ox, oy, 2.8, 3.4));
    builder.line_to(pt(ox, oy, 6.1, 3.4));
    builder.line_to(pt(ox, oy, 7.7, 5.0));
    builder.line_to(pt(ox, oy, 13.2, 5.0));
    builder.line_to(pt(ox, oy, 14.0, 5.8));
    builder.line_to(pt(ox, oy, 14.0, 12.1));
    builder.line_to(pt(ox, oy, 13.2, 12.9));
    builder.line_to(pt(ox, oy, 2.8, 12.9));
    builder.line_to(pt(ox, oy, 2.0, 12.1));
    builder.close();
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// Renders a crisp `14×16 px` tree disclosure chevron (`>` when collapsed, `⌄` when expanded).
#[must_use]
pub fn render_chevron(expanded: bool, color: Hsla) -> AnyElement {
    vector_icon(14.0, 16.0, move |ox, oy, window| {
        if expanded {
            stroke_segments(
                window,
                1.3,
                color,
                &[&[(3.5, 6.2), (7.0, 9.7), (10.5, 6.2)]],
                ox,
                oy,
            );
        } else {
            stroke_segments(
                window,
                1.3,
                color,
                &[&[(5.4, 4.5), (9.0, 8.0), (5.4, 11.5)]],
                ox,
                oy,
            );
        }
    })
}

/// Renders a compact `10×10 px` downward chevron (`⌄`) for the `Project ⌄` header.
#[must_use]
pub fn render_header_chevron(color: Hsla) -> AnyElement {
    vector_icon(10.0, 10.0, move |ox, oy, window| {
        stroke_segments(
            window,
            1.3,
            color,
            &[&[(2.0, 3.6), (5.0, 6.6), (8.0, 3.6)]],
            ox,
            oy,
        );
    })
}

/// Renders a `16×16 px` outlined `IntelliJ` New UI folder icon, optionally with the blue
/// module badge in the bottom-right corner (`is_root == true`).
#[must_use]
pub fn render_folder_icon(
    is_root: bool,
    color: Hsla,
    badge_bg: Hsla,
    island_bg: Hsla,
) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        paint_folder_outline(ox, oy, window, 1.3, color);

        if is_root {
            paint_rounded_rect(window, (ox, oy), (8.6, 8.6, 7.2, 7.2), 2.2, island_bg);
            paint_rounded_rect(window, (ox, oy), (9.6, 9.6, 5.4, 5.4), 1.5, badge_bg);
        }
    })
}

/// Renders a `16×16 px` semantic file icon for `kind` using `palette` (or warm amber when `git_ignored`).
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "vector path definitions for each FileIconKind are concise and self-contained"
)]
pub fn render_file_icon(
    kind: FileIconKind,
    palette: &ProjectIconPalette,
    git_ignored: bool,
) -> AnyElement {
    let p = *palette;
    let tint = move |c: Hsla| if git_ignored { p.git_ignored } else { c };

    vector_icon(16.0, 16.0, move |ox, oy, window| match kind {
        FileIconKind::Markdown => {
            let color = tint(p.markdown);
            // Crisp `M` on the left + `↓` on the right (`ref_intellij_project_1.png`).
            stroke_segments(
                window,
                1.5,
                color,
                &[
                    &[(2.2, 11.6), (2.2, 4.6), (5.0, 8.2), (7.8, 4.6), (7.8, 11.6)],
                    &[(11.8, 4.3), (11.8, 11.3)],
                    &[(9.8, 9.2), (11.8, 11.5), (13.8, 9.2)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Java => {
            let color = tint(p.java);
            // Stylized coffee cup + steam wisps.
            stroke_segments(
                window,
                1.25,
                color,
                &[
                    &[
                        (4.0, 6.5),
                        (11.0, 6.5),
                        (10.3, 12.2),
                        (4.7, 12.2),
                        (4.0, 6.5),
                    ],
                    &[(11.0, 7.4), (13.0, 7.4), (13.0, 10.2), (10.6, 10.2)],
                    &[(3.4, 13.6), (11.6, 13.6)],
                    &[(5.8, 4.8), (6.5, 3.0)],
                    &[(8.5, 4.8), (9.2, 3.0)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::JavaTest => {
            let color = tint(p.java_test);
            paint_outlined_rounded_rect(window, (ox, oy), (2.5, 2.5, 11.0, 11.0), 2.5, 1.25, color);
            fill_polygon(
                window,
                color,
                &[(6.4, 5.2), (11.0, 8.0), (6.4, 10.8)],
                ox,
                oy,
            );
        }
        FileIconKind::Kotlin => {
            let c1 = tint(p.kotlin_primary);
            let c2 = tint(p.kotlin_accent);
            fill_polygon(
                window,
                c1,
                &[(3.0, 3.0), (13.0, 3.0), (8.0, 8.0), (3.0, 13.0)],
                ox,
                oy,
            );
            fill_polygon(
                window,
                c2,
                &[(8.0, 8.0), (13.0, 3.0), (13.0, 13.0), (3.0, 13.0)],
                ox,
                oy,
            );
        }
        FileIconKind::Yaml => {
            let color = tint(p.yaml);
            // Coral-red YAML exclamation/key-value mark.
            stroke_segments(
                window,
                1.5,
                color,
                &[
                    &[(4.2, 3.6), (4.2, 9.4)],
                    &[(7.8, 5.2), (13.0, 5.2)],
                    &[(7.8, 8.2), (12.0, 8.2)],
                    &[(7.8, 11.2), (13.0, 11.2)],
                ],
                ox,
                oy,
            );
            paint_rounded_rect(window, (ox, oy), (3.3, 11.0, 1.8, 1.8), 0.9, color);
        }
        FileIconKind::Xml => {
            let color = tint(p.xml);
            stroke_segments(
                window,
                1.35,
                color,
                &[
                    &[(5.6, 4.4), (2.2, 8.0), (5.6, 11.6)],
                    &[(10.4, 4.4), (13.8, 8.0), (10.4, 11.6)],
                    &[(8.9, 4.2), (7.1, 11.8)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Groovy => {
            let color = tint(p.groovy);
            // 5-point star outline.
            stroke_segments(
                window,
                1.25,
                color,
                &[&[
                    (8.0, 2.3),
                    (9.6, 6.0),
                    (13.6, 6.3),
                    (10.5, 9.0),
                    (11.5, 13.0),
                    (8.0, 10.9),
                    (4.5, 13.0),
                    (5.5, 9.0),
                    (2.4, 6.3),
                    (6.4, 6.0),
                    (8.0, 2.3),
                ]],
                ox,
                oy,
            );
        }
        FileIconKind::Rust => {
            let color = tint(p.rust);
            paint_outlined_rounded_rect(window, (ox, oy), (3.6, 3.6, 8.8, 8.8), 4.4, 1.5, color);
            stroke_segments(
                window,
                1.4,
                color,
                &[
                    &[(8.0, 1.8), (8.0, 3.7)],
                    &[(8.0, 12.3), (8.0, 14.2)],
                    &[(1.8, 8.0), (3.7, 8.0)],
                    &[(12.3, 8.0), (14.2, 8.0)],
                    &[(3.6, 3.6), (5.0, 5.0)],
                    &[(11.0, 11.0), (12.4, 12.4)],
                    &[(12.4, 3.6), (11.0, 5.0)],
                    &[(5.0, 11.0), (3.6, 12.4)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Json => {
            let color = tint(p.json);
            stroke_segments(
                window,
                1.35,
                color,
                &[
                    &[
                        (6.0, 3.2),
                        (4.4, 3.2),
                        (4.4, 6.8),
                        (3.0, 8.0),
                        (4.4, 9.2),
                        (4.4, 12.8),
                        (6.0, 12.8),
                    ],
                    &[
                        (10.0, 3.2),
                        (11.6, 3.2),
                        (11.6, 6.8),
                        (13.0, 8.0),
                        (11.6, 9.2),
                        (11.6, 12.8),
                        (10.0, 12.8),
                    ],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Toml => {
            let color = tint(p.toml);
            stroke_segments(
                window,
                1.35,
                color,
                &[
                    &[(5.0, 3.2), (2.8, 3.2), (2.8, 12.8), (5.0, 12.8)],
                    &[(11.0, 3.2), (13.2, 3.2), (13.2, 12.8), (11.0, 12.8)],
                    &[(5.8, 5.5), (10.2, 5.5)],
                    &[(8.0, 5.5), (8.0, 11.0)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::KovaConfig => {
            let color = tint(p.kova_config);
            paint_outlined_rounded_rect(window, (ox, oy), (4.0, 4.0, 8.0, 8.0), 4.0, 1.5, color);
            paint_rounded_rect(window, (ox, oy), (6.8, 6.8, 2.4, 2.4), 1.2, color);
            stroke_segments(
                window,
                1.4,
                color,
                &[
                    &[(8.0, 2.0), (8.0, 3.8)],
                    &[(8.0, 12.2), (8.0, 14.0)],
                    &[(2.0, 8.0), (3.8, 8.0)],
                    &[(12.2, 8.0), (14.0, 8.0)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Http => {
            let c1 = tint(p.http_primary);
            let c2 = tint(p.http_accent);
            stroke_segments(
                window,
                1.35,
                c1,
                &[
                    &[(3.0, 5.8), (12.5, 5.8)],
                    &[(9.8, 3.6), (12.6, 5.8), (9.8, 8.0)],
                ],
                ox,
                oy,
            );
            stroke_segments(
                window,
                1.35,
                c2,
                &[
                    &[(13.0, 10.4), (3.5, 10.4)],
                    &[(6.2, 8.2), (3.4, 10.4), (6.2, 12.6)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::Shell => {
            let color = tint(p.shell);
            paint_outlined_rounded_rect(window, (ox, oy), (2.2, 3.0, 11.6, 10.0), 2.0, 1.2, color);
            stroke_segments(
                window,
                1.3,
                color,
                &[
                    &[(4.5, 6.0), (7.0, 8.0), (4.5, 10.0)],
                    &[(8.2, 10.2), (11.2, 10.2)],
                ],
                ox,
                oy,
            );
        }
        FileIconKind::DotConfig => {
            let color = tint(p.default_file);
            paint_outlined_rounded_rect(window, (ox, oy), (4.2, 4.2, 7.6, 7.6), 3.8, 1.25, color);
            paint_rounded_rect(window, (ox, oy), (6.8, 6.8, 2.4, 2.4), 1.2, color);
        }
        FileIconKind::Generic => {
            let color = tint(p.default_file);
            stroke_segments(
                window,
                1.2,
                color,
                &[
                    &[
                        (3.6, 2.5),
                        (9.6, 2.5),
                        (12.6, 5.5),
                        (12.6, 13.5),
                        (3.6, 13.5),
                        (3.6, 2.5),
                    ],
                    &[(9.6, 2.5), (9.6, 5.5), (12.6, 5.5)],
                ],
                ox,
                oy,
            );
        }
    })
}

/// Renders the `16×16 px` `+` (`New...`) toolbar icon with a small bottom-right dropdown triangle.
#[must_use]
pub fn render_new_item_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        stroke_segments(
            window,
            1.35,
            color,
            &[&[(7.5, 3.2), (7.5, 11.8)], &[(3.2, 7.5), (11.8, 7.5)]],
            ox,
            oy,
        );
        fill_polygon(
            window,
            color,
            &[(11.5, 14.0), (14.2, 14.0), (14.2, 11.3)],
            ox,
            oy,
        );
    })
}

/// Renders the `16×16 px` crosshair (`Select Opened File`) toolbar icon.
#[must_use]
pub fn render_locate_file_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        paint_outlined_rounded_rect(window, (ox, oy), (3.2, 3.2, 9.6, 9.6), 4.8, 1.2, color);
        stroke_segments(
            window,
            1.2,
            color,
            &[
                &[(8.0, 1.5), (8.0, 4.6)],
                &[(8.0, 11.4), (8.0, 14.5)],
                &[(1.5, 8.0), (4.6, 8.0)],
                &[(11.4, 8.0), (14.5, 8.0)],
            ],
            ox,
            oy,
        );
        paint_rounded_rect(window, (ox, oy), (7.1, 7.1, 1.8, 1.8), 0.9, color);
    })
}

/// Renders the `16×16 px` `Expand All` (`^` above `v`) toolbar icon.
#[must_use]
pub fn render_expand_all_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        stroke_segments(
            window,
            1.3,
            color,
            &[
                &[(4.8, 6.2), (8.0, 3.0), (11.2, 6.2)],
                &[(4.8, 9.8), (8.0, 13.0), (11.2, 9.8)],
            ],
            ox,
            oy,
        );
    })
}

/// Renders the `16×16 px` `Collapse All` (`v` above `^`) toolbar icon.
#[must_use]
pub fn render_collapse_all_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        stroke_segments(
            window,
            1.3,
            color,
            &[
                &[(4.8, 3.4), (8.0, 6.6), (11.2, 3.4)],
                &[(4.8, 12.6), (8.0, 9.4), (11.2, 12.6)],
            ],
            ox,
            oy,
        );
    })
}

/// Renders the `16×16 px` `Options` (`⋮`) toolbar icon.
#[must_use]
pub fn render_options_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        for cy in [4.0, 8.0, 12.0] {
            paint_rounded_rect(window, (ox, oy), (7.0, cy - 1.0, 2.0, 2.0), 1.0, color);
        }
    })
}

/// Renders the `16×16 px` `Hide` (`—`) toolbar icon.
#[must_use]
pub fn render_hide_icon(color: Hsla) -> AnyElement {
    vector_icon(16.0, 16.0, move |ox, oy, window| {
        stroke_segments(window, 1.35, color, &[&[(3.5, 8.0), (12.5, 8.0)]], ox, oy);
    })
}

/// Renders a crisp `14×14 px` hammer (`Build Project`) vector icon for the titlebar toolbar.
#[must_use]
pub fn render_build_hammer_icon(color: Hsla) -> AnyElement {
    vector_icon(14.0, 14.0, move |ox, oy, window| {
        // Angled hammer head + diagonal handle.
        fill_polygon(
            window,
            color,
            &[(2.2, 5.2), (5.4, 2.0), (8.8, 5.4), (5.6, 8.6)],
            ox,
            oy,
        );
        fill_polygon(
            window,
            color,
            &[(7.2, 4.0), (9.4, 1.8), (10.5, 2.9), (8.3, 5.1)],
            ox,
            oy,
        );
        stroke_segments(window, 2.1, color, &[&[(6.8, 6.8), (11.8, 11.8)]], ox, oy);
    })
}

/// Renders a crisp `12×12 px` green play triangle (`Run`) vector icon.
#[must_use]
pub fn render_run_play_icon(color: Hsla) -> AnyElement {
    vector_icon(12.0, 12.0, move |ox, oy, window| {
        fill_polygon(
            window,
            color,
            &[(2.5, 1.8), (10.2, 6.0), (2.5, 10.2)],
            ox,
            oy,
        );
    })
}

/// Renders a `14×14 px` `Run All Tests` vector icon (play triangle + check accent).
#[must_use]
pub fn render_run_tests_icon(color: Hsla) -> AnyElement {
    vector_icon(14.0, 14.0, move |ox, oy, window| {
        fill_polygon(window, color, &[(1.8, 2.2), (8.2, 6.0), (1.8, 9.8)], ox, oy);
        stroke_segments(
            window,
            1.5,
            color,
            &[&[(7.2, 10.2), (9.2, 12.2), (12.8, 7.6)]],
            ox,
            oy,
        );
    })
}

/// Renders a `12×12 px` stop square (`■`) vector icon.
#[must_use]
pub fn render_stop_icon(color: Hsla) -> AnyElement {
    vector_icon(12.0, 12.0, move |ox, oy, window| {
        paint_rounded_rect(window, (ox, oy), (2.2, 2.2, 7.6, 7.6), 1.4, color);
    })
}
