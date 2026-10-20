// The built-in background and border, written against the same contract as
// user paint shaders.
vec4 paint_main(PaintContext ctx) {
    vec4 color = vec4(0.0);
    if (shoji_box_fill > 0.5) {
        color = shoji_fill_color(ctx) * shoji_fill_coverage(ctx);
    }
    if (shoji_box_border > 0.5) {
        color = shoji_over(shoji_border_color_at(ctx) * shoji_border_coverage(ctx), color);
    }
    return color;
}
