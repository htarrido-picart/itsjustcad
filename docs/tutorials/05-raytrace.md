# Tutorial 5 — Render a massing with the ray tracer

*You will build a small massing, aim the camera, and render it to a photoreal PNG
with the built-in path tracer — real materials, sun shadows, and global
illumination, no external tools. About 10 minutes.*

The ray tracer renders your **actual model** accurately. That is different from the
[AI diffusion render](06-parametric.md) (`render`), which reimagines the view from a
text prompt. Use the ray tracer when you want a faithful picture of what you built.

---

## 1. Build something to render

Start from a fresh document (⌘N / Ctrl+N) and type each line into the command bar:

```
box 0,0,0 40,30,4
name last podium
box 6,6,4 20,14,24
name last tower
```

Give the surfaces a look so the render has materials to work with:

```
material2 podium concrete
material2 tower glass
```

Add a ground plane so shadows have something to land on:

```
rect -20,-20,0 100 100
name last ground
material2 ground concrete
```

---

## 2. Aim the camera

The ray tracer renders whatever the **active viewport** is looking at, so set up a
good perspective view first:

```
persp
ze
```

Orbit with right-mouse drag until the massing looks the way you want. A low sun
angle gives you long, legible shadows.

```
sun 40.71 -74.01 2024-06-21 09:00
```

The viewport now shows the real solar direction; the ray tracer will use the same
sun for its shadows.

---

## 3. Open the progressive render window

Choose **Render ▸ Raytrace…** from the menu bar. A window opens with a **live
preview** that starts noisy and refines as samples accumulate. The controls let you
set:

- **Samples** — more samples = less noise, slower. Start around 48–128.
- **Bounces** — light-path depth (higher = more accurate GI, slower).
- **Resolution** — output size.
- **Sun / Sky** — direction and sky brightness.

Press **Stop** when the preview looks clean enough, then **Save PNG** to write the
image.

---

## 4. Render from the command line (optional)

You can also render straight to a file without opening the window — handy for
scripts and headless runs. The `raytrace` verb takes optional positional args:

```
raytrace                       # → raytrace.png, default samples & size
raytrace dusk.png 256          # 256 samples per pixel
raytrace hero.png 128 1600     # 128 spp at 1600 px wide
```

The arguments are order-tolerant: a filename sets the output, a small number is the
sample count, and a large number is the image width. Because the path tracer is pure
CPU it runs **headless** too:

```sh
itsjustcad --run scene.txt --headless
# with a `raytrace hero.png 128 1600` line in the script
```

---

## You now have

A photoreal, physically-based render of your real model — accurate materials, sun
shadows, and global illumination — saved as a PNG, produced entirely in-app with no
external renderer.

Next: [edit a live parametric structure](06-parametric.md).
