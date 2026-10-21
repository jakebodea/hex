#include <metal_stdlib>
using namespace metal;

struct Uniforms {
    float2 resolution;
    float time;
    float width;
    float height;
    float opacity;
    float scale;
    float softness;
    float average;
    float peak;
    float processing;
    float post_processing;
    float capturing;
    float editing;
    float queued_count;
    float line_style;
    float line_count;
    float line_curvature;
    float line_speed;
    float line_sharpness;
    float line_glow;
    float sphere_depth;
    float light_angle;
    float sphere_outline;
    float completion;
    float recording_flash;
    float level_phase;
    float levels[8];
};

struct VertexOutput {
    float4 position [[position]];
    float2 uv;
};

vertex VertexOutput indicator_vertex(uint vertex_id [[vertex_id]]) {
    const float2 positions[] = {
        float2(-1.0, -1.0),
        float2( 3.0, -1.0),
        float2(-1.0,  3.0),
    };
    VertexOutput output;
    output.position = float4(positions[vertex_id], 0.0, 1.0);
    output.uv = positions[vertex_id] * float2(0.5, -0.5) + 0.5;
    return output;
}

float rounded_box(float2 point, float2 half_size, float radius) {
    float2 q = abs(point) - half_size + radius;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - radius;
}

float coverage(float distance, float softness) {
    return smoothstep(softness, -softness, distance);
}

float glow(float distance, float radius) {
    float outside = max(distance, 0.0);
    return exp2(-outside * outside / max(radius * radius, 0.001));
}

void composite(thread float4 &destination, float3 color, float alpha) {
    alpha = clamp(alpha, 0.0, 1.0);
    destination.rgb = color * alpha + destination.rgb * (1.0 - alpha);
    destination.a = alpha + destination.a * (1.0 - alpha);
}

void screen(thread float4 &destination, float3 color, float amount) {
    amount = clamp(amount, 0.0, 1.0);
    destination.rgb = 1.0 - (1.0 - destination.rgb) * (1.0 - color * amount);
}

float hash(float value) {
    return fract(sin(value * 12.9898) * 43758.5453);
}

// Loudness at a distance (in bars) from the center: the center follows the
// live meter and older samples ripple outward, interpolated between frames.
// Every bar keeps a live component so the ripple never conceals speech onset.
float ripple_level(constant Uniforms &uniforms, float distance_in_bars) {
    if (distance_in_bars < 0.5) {
        return clamp(uniforms.average, 0.0, 1.0);
    }
    float position = clamp(distance_in_bars - uniforms.level_phase, 0.0, 7.0);
    int lower = int(floor(position));
    int upper = min(lower + 1, 7);
    float history = mix(uniforms.levels[lower], uniforms.levels[upper], fract(position));
    return mix(history, clamp(uniforms.average, 0.0, 1.0), 0.65);
}

fragment float4 indicator_fragment(
    VertexOutput input [[stage_in]],
    constant Uniforms &uniforms [[buffer(0)]])
{
    float backing_scale = uniforms.resolution.x / 136.0;
    float2 point = (input.uv * uniforms.resolution - uniforms.resolution * 0.5)
        / backing_scale / max(uniforms.scale, 0.001);
    float2 half_size = float2(uniforms.width, uniforms.height) * 0.5;
    float radius = uniforms.height * 0.5;
    float distance = rounded_box(point, half_size, radius);
    float lifecycle_softness = max(uniforms.softness, 0.0);
    float edge = (max(fwidth(distance), 0.32) + lifecycle_softness * 0.72)
        / max(uniforms.scale, 0.001);
    float shape = coverage(distance, edge);
    float detail_clarity = exp2(-lifecycle_softness * 0.34);
    float4 result = 0.0;

    float processing = clamp(uniforms.processing, 0.0, 1.0);
    float post_processing = clamp(uniforms.post_processing, 0.0, 1.0);
    float editing = clamp(uniforms.editing, 0.0, 1.0);
    float completion = clamp(uniforms.completion, 0.0, 1.0);
    float recording = 1.0 - processing;
    float recording_flash = clamp(uniforms.recording_flash, 0.0, 1.0) * recording;
    float completion_flash = smoothstep(0.0, 0.16, completion)
        * (1.0 - smoothstep(0.32, 1.0, completion));

    // Soft drop shadow so the pill floats over light and dark windows alike.
    float shadow_distance = rounded_box(point - float2(0.0, 1.5), half_size, radius);
    composite(result, float3(0.0), glow(shadow_distance, 5.0) * 0.32 * (1.0 - shape));

    // Near-black frosted body with a hairline top highlight.
    float vertical = clamp(point.y / max(uniforms.height, 0.001) + 0.5, 0.0, 1.0);
    float3 body = mix(float3(0.075, 0.075, 0.085), float3(0.035, 0.035, 0.04), vertical);
    composite(result, body, shape * 0.94);
    float hairline = coverage(abs(distance + 0.5) - 0.5, 0.45);
    float3 hairline_color = mix(float3(0.34), float3(0.2), vertical);
    composite(result, hairline_color, hairline * 0.55 * detail_clarity);

    float3 ink = mix(float3(0.96, 0.96, 0.98), float3(0.42, 0.95, 0.72), editing);

    // Recording: white bars that ripple outward from the center with the voice.
    if (recording > 0.001) {
        const float bar_count = 15.0;
        const float pitch = 3.6;
        const float bar_width = 2.0;
        float half_span = (bar_count - 1.0) * pitch * 0.5;
        float slot = clamp(round((point.x + half_span) / pitch), 0.0, bar_count - 1.0);
        float center_x = slot * pitch - half_span;
        float from_center = abs(slot - (bar_count - 1.0) * 0.5);
        float envelope = 1.0 - 0.5 * pow(from_center / 7.0, 2.0);
        float wobble = 0.82 + 0.18 * sin(uniforms.time * 7.0 + hash(slot) * 6.2831853);
        float level = pow(ripple_level(uniforms, from_center), 0.7) * envelope * wobble;
        float max_half_height = uniforms.height * 0.5 - 5.0;
        float idle = 0.85 + 0.25 * sin(uniforms.time * 3.0 - from_center * 0.7);
        float half_height = max(idle, level * max_half_height);
        float bar = rounded_box(
            point - float2(center_x, 0.0),
            float2(bar_width * 0.5, half_height),
            bar_width * 0.5);
        float bar_mask = coverage(bar, max(fwidth(bar), 0.3)) * shape;
        float brightness = mix(0.55, 1.0, smoothstep(0.02, 0.35, level));
        composite(result, ink, bar_mask * brightness * recording * detail_clarity);
    }

    // Working: three dots in a travelling bounce; violet while AI rewrites.
    if (processing > 0.001) {
        float3 dot_color = mix(float3(0.92, 0.93, 0.96), float3(0.72, 0.6, 1.0), post_processing);
        for (int index = 0; index < 3; index++) {
            float phase = uniforms.time * 6.0 - float(index) * 0.75;
            float bounce = max(sin(phase), 0.0);
            float2 center = float2((float(index) - 1.0) * 6.5, -bounce * 2.6);
            float dot_distance = length(point - center) - 1.9;
            float dot_mask = coverage(dot_distance, max(fwidth(dot_distance), 0.28)) * shape;
            float alpha = mix(0.45, 1.0, bounce);
            composite(result, dot_color, dot_mask * alpha * processing * detail_clarity);
        }
    }

    // Pending jobs sit beside the pill as small dots.
    float queued_count = min(uniforms.queued_count, 4.0);
    for (int index = 0; index < 4; index++) {
        if (float(index) >= queued_count) {
            break;
        }
        // A dark backing keeps the dot legible over light windows too.
        float2 center = float2(uniforms.width * 0.5 + 8.0 + float(index) * 6.5, 0.0);
        float backing_distance = length(point - center) - 2.8;
        composite(result, float3(0.06), coverage(backing_distance, 0.3) * 0.9);
        float dot_distance = length(point - center) - 1.2;
        float dot = coverage(dot_distance, 0.25);
        composite(result, float3(0.9), dot * (index == 0 ? 0.9 : 0.5));
    }

    // Start and finish acknowledgements: a brief bright rim.
    float rim = exp2(-pow(abs(distance) / 0.8, 2.0));
    screen(result, mix(float3(1.0), ink, editing), rim * recording_flash * 0.55);
    screen(result, float3(0.85, 0.9, 1.0), rim * completion_flash * 0.6);

    result *= uniforms.opacity;
    if (result.a > 0.0) {
        float2 pixel = floor(input.position.xy);
        float n1 = fract(52.9829189 * fract(dot(pixel, float2(0.06711056, 0.00583715))));
        float n2 = fract(
            52.9829189 * fract(dot(pixel + float2(17.0, 31.0), float2(0.06711056, 0.00583715))));
        float dither = ((n1 - n2) / 255.0) * result.a;
        result.rgb = clamp(result.rgb + float3(dither), float3(0.0), float3(result.a));
    }
    return result;
}
