//! Backdrop filter pass.

use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use egui::{pos2, Mesh, Rect, Ui, Vec2, TextureId};
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Vertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
    pub alpha: f32,
}

#[derive(Clone, Copy, Debug)]
pub enum Filter {
    Invert = 0,
    Grayscale = 1,
}

#[derive(Default, Debug)]
pub struct FilterBatch {
    verts: Vec<Vertex>,
    tex_id: Option<TextureId>,
}

impl FilterBatch {
    pub fn is_empty(&self) -> bool {
        self.verts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.verts.len()
    }

    pub fn v(&mut self, x: f32, y: f32, u: f32, v: f32, alpha: f32) {
        self.verts.push(Vertex { pos: [x, y], uv: [u, v], alpha });
    }

    pub fn rect(&mut self, r: Rect, alpha: f32) {
        if r.width() <= 0.0 || r.height() <= 0.0 {
            return;
        }
        let (a, b, c, d) = (r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom());
        for p in [a, b, c, a, c, d] {
            self.v(p.x, p.y, -1.0, -1.0, alpha);
        }
    }

    pub fn frame(&mut self, r: Rect, t: f32, alpha: f32) {
        self.rect(Rect::from_min_max(r.min, pos2(r.max.x, r.min.y + t)), alpha);
        self.rect(Rect::from_min_max(pos2(r.min.x, r.max.y - t), r.max), alpha);
        self.rect(Rect::from_min_max(pos2(r.min.x, r.min.y + t), pos2(r.min.x + t, r.max.y - t)), alpha);
        self.rect(Rect::from_min_max(pos2(r.max.x - t, r.min.y + t), pos2(r.max.x, r.max.y - t)), alpha);
    }

    /// Add a text mesh as mask geometry.
    ///
    /// egui stores text mesh UVs as raw font-atlas pixel coordinates, not
    /// normalized [0,1]. epaint itself normalizes at paint time; we have to do
    /// the same so the shader's `texture(u_font, uv)` samples the right place.
    pub fn text_mesh(&mut self, mesh: &Mesh, offset: Vec2, atlas_size: [u32; 2], alpha: f32) {
        if self.tex_id.is_none() {
            self.tex_id = Some(mesh.texture_id);
        }
        let (ax, ay) = (atlas_size[0] as f32, atlas_size[1] as f32);
        if ax <= 0.0 || ay <= 0.0 {
            return;
        }
        for tri in mesh.indices.chunks_exact(3) {
            for &i in tri {
                let vx = &mesh.vertices[i as usize];
                self.v(
                    vx.pos.x + offset.x,
                    vx.pos.y + offset.y,
                    vx.uv.x / ax,
                    vx.uv.y / ay,
                    alpha,
                );
            }
        }
    }
}

const VS: &str = r#"#version 330 core
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
layout(location = 2) in float a_alpha;
uniform vec2 u_screen_pts;
out vec2 v_uv;
out float v_alpha;
void main() {
v_uv = a_uv;
v_alpha = a_alpha;
gl_Position = vec4(a_pos.x / u_screen_pts.x * 2.0 - 1.0,
1.0 - a_pos.y / u_screen_pts.y * 2.0, 0.0, 1.0);
}
"#;

const FS: &str = r#"#version 330 core
in vec2 v_uv;
in float v_alpha;
uniform sampler2D u_font;
uniform sampler2D u_back;
uniform vec2 u_screen_px;
uniform int u_filter;
out vec4 o_color;

vec3 apply(vec3 c) {
if (u_filter == 1) {
    float g = dot(c, vec3(0.299, 0.587, 0.114));
    return vec3(g);
    }
    return vec3(1.0) - c;
    }

    void main() {
    float m;
    if (v_uv.x < 0.0) {
        m = 1.0;
        } else {
            m = texture(u_font, v_uv).a;
            }
            m *= v_alpha;
            vec4 back = texture(u_back, gl_FragCoord.xy / u_screen_px);
            o_color = vec4(mix(back.rgb, apply(back.rgb), m), back.a);
            }
            "#;

            struct GlState {
                prog: glow::Program,
                vao: glow::VertexArray,
                vbo: glow::Buffer,
                back: glow::Texture,
                back_size: [i32; 2],
                loc_screen_pts: Option<glow::UniformLocation>,
                loc_screen_px: Option<glow::UniformLocation>,
                loc_font: Option<glow::UniformLocation>,
                loc_back: Option<glow::UniformLocation>,
                loc_filter: Option<glow::UniformLocation>,
            }

            impl GlState {
                unsafe fn new(gl: &glow::Context) -> Self {
                    let prog = gl.create_program().expect("create program");
                    let mut shaders = Vec::new();
                    for (ty, src) in [(glow::VERTEX_SHADER, VS), (glow::FRAGMENT_SHADER, FS)] {
                        let sh = gl.create_shader(ty).expect("create shader");
                        gl.shader_source(sh, src);
                        gl.compile_shader(sh);
                        if !gl.get_shader_compile_status(sh) {
                            eprintln!("filter: shader compile error: {}", gl.get_shader_info_log(sh));
                        }
                        gl.attach_shader(prog, sh);
                        shaders.push(sh);
                    }
                    gl.link_program(prog);
                    if !gl.get_program_link_status(prog) {
                        eprintln!("filter: program link error: {}", gl.get_program_info_log(prog));
                    }
                    for sh in shaders {
                        gl.detach_shader(prog, sh);
                        gl.delete_shader(sh);
                    }

                    let vao = gl.create_vertex_array().expect("create vao");
                    let vbo = gl.create_buffer().expect("create vbo");
                    gl.bind_vertex_array(Some(vao));
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
                    let stride = std::mem::size_of::<Vertex>() as i32;
                    gl.enable_vertex_attrib_array(0);
                    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0);
                    gl.enable_vertex_attrib_array(1);
                    gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8);
                    gl.enable_vertex_attrib_array(2);
                    gl.vertex_attrib_pointer_f32(2, 1, glow::FLOAT, false, stride, 16);

                    let back = gl.create_texture().expect("create texture");
                    gl.bind_texture(glow::TEXTURE_2D, Some(back));
                    for (p, v) in [
                        (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
                        (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
                        (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                        (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                    ] {
                        gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
                    }

                    Self {
                        loc_screen_pts: gl.get_uniform_location(prog, "u_screen_pts"),
                        loc_screen_px: gl.get_uniform_location(prog, "u_screen_px"),
                        loc_font: gl.get_uniform_location(prog, "u_font"),
                        loc_back: gl.get_uniform_location(prog, "u_back"),
                        loc_filter: gl.get_uniform_location(prog, "u_filter"),
                        prog,
                        vao,
                        vbo,
                        back,
                        back_size: [0, 0],
                    }
                }

                unsafe fn draw(
                    &mut self,
                    gl: &glow::Context,
                    painter: &egui_glow::Painter,
                    info: &egui::PaintCallbackInfo,
                    verts: &[Vertex],
                    tex_id: TextureId,
                    filter: Filter,
                ) {
                    let Some(font_tex) = painter.texture(tex_id) else {
                        eprintln!("filter: texture {:?} not found", tex_id);
                        return;
                    };
                    let [sw, sh] = info.screen_size_px;
                    let ppp = info.pixels_per_point;

                    let mut prev_vp = [0i32; 4];
                    gl.get_parameter_i32_slice(glow::VIEWPORT, &mut prev_vp);
                    let prev_scissor = gl.is_enabled(glow::SCISSOR_TEST);
                    gl.viewport(0, 0, sw as i32, sh as i32);
                    gl.disable(glow::SCISSOR_TEST);

                    gl.active_texture(glow::TEXTURE1);
                    gl.bind_texture(glow::TEXTURE_2D, Some(self.back));
                    let size = [sw as i32, sh as i32];
                    if size == self.back_size {
                        gl.copy_tex_sub_image_2d(glow::TEXTURE_2D, 0, 0, 0, 0, 0, sw as i32, sh as i32);
                    } else {
                        gl.copy_tex_image_2d(glow::TEXTURE_2D, 0, glow::RGBA, 0, 0, sw as i32, sh as i32, 0);
                        self.back_size = size;
                    }

                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(font_tex));

                    gl.use_program(Some(self.prog));
                    gl.uniform_2_f32(self.loc_screen_pts.as_ref(), sw as f32 / ppp, sh as f32 / ppp);
                    gl.uniform_2_f32(self.loc_screen_px.as_ref(), sw as f32, sh as f32);
                    gl.uniform_1_i32(self.loc_font.as_ref(), 0);
                    gl.uniform_1_i32(self.loc_back.as_ref(), 1);
                    gl.uniform_1_i32(self.loc_filter.as_ref(), filter as i32);

                    gl.disable(glow::BLEND);
                    gl.bind_vertex_array(Some(self.vao));
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
                    let bytes = std::slice::from_raw_parts(
                        verts.as_ptr() as *const u8,
                                                           verts.len() * std::mem::size_of::<Vertex>(),
                    );
                    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
                    gl.draw_arrays(glow::TRIANGLES, 0, verts.len() as i32);
                    gl.enable(glow::BLEND);
                    gl.active_texture(glow::TEXTURE0);

                    gl.viewport(prev_vp[0], prev_vp[1], prev_vp[2], prev_vp[3]);
                    if prev_scissor {
                        gl.enable(glow::SCISSOR_TEST);
                    }
                }
            }

            #[derive(Default)]
            pub struct FilterPass {
                gl: Arc<Mutex<Option<GlState>>>,
            }

            impl FilterPass {
                pub fn paint(&self, ui: &Ui, batch: FilterBatch, filter: Filter) {
                    if batch.is_empty() {
                        return;
                    }
                    let verts = Arc::new(batch.verts);
                    let tex_id = batch.tex_id.unwrap_or(TextureId::Managed(0));
                    let state = self.gl.clone();
                    let cb = egui_glow::CallbackFn::new(move |info, painter| {
                        let gl: &glow::Context = &**painter.gl();
                        let mut guard = state.lock().unwrap();
                        let st = guard.get_or_insert_with(|| unsafe { GlState::new(gl) });
                        unsafe { st.draw(gl, painter, &info, &verts, tex_id, filter) };
                    });
                    ui.painter().add(egui::Shape::Callback(egui::PaintCallback {
                        rect: ui.ctx().screen_rect(),
                                                           callback: Arc::new(cb),
                    }));
                }
            }
