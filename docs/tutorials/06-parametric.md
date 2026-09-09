# Tutorial 6 — Edit a parametric structure

*You will create a geodesic dome, tune it live from the Parameters tab and from the
command line, then freeze it to a plain mesh when you are happy. About 10 minutes.
The last section sets up a fully local, offline AI render.*

Built-in parametric structures — geodesic domes, hypar shells, gaussvaults,
gridshells, funiculars, tensegrity, cablenets, space frames — **stay editable after
you create them**. Change a parameter and the geometry re-derives instantly.

---

## 1. Create a parametric structure

From a fresh document (⌘N / Ctrl+N):

```
geodesic 3 5 dome
```

That builds a frequency-3 geodesic dome of radius 5. Because it is parametric, it
now appears as a **card in the Parameters tab** of the right dock (open the dock and
select **Parameters** if it is not showing).

---

## 2. Edit it live in the Parameters tab

Each card carries a schema-driven editor — sliders, numeric fields, dropdowns, and
toggles, one per parameter. Drag the **frequency** slider up and the dome re-tessellates
in real time; change the **radius** and it rescales. Nothing is baked yet, so you can
keep adjusting until it reads the way you want.

The editor knows each parameter's type and range from the structure's schema, so you
only ever get valid values.

---

## 3. Edit it from the command line

The same edits are available as a verb — useful in scripts and from the deck:

```
paramset last frequency=4
paramset last radius=6
```

Each `paramset` re-derives the mesh immediately, exactly like moving a slider. This
works on every parametric generator — for example a Dieste-style vault:

```
gaussvault 8 20 3
paramset last rise=5
```

---

## 4. Freeze it when you're done

Once the shape is final, flatten it to a plain static mesh. This keeps the geometry
but drops the generator and its parameters, so the card leaves the Parameters tab and
the object is no longer editable:

```
freeze last
```

(`bake` is an alias for `freeze`.) Freeze when you want to boolean it, export it, or
simply lock the design.

---

## 5. (Optional) Set up a fully local AI render

If you also want the AI diffusion render to run **offline on your own machine**, open
the **Render Setup** panel. It walks you through two things:

1. **Download a local Stable Diffusion model** from the built-in model catalog.
2. **Detect the `sd` binary** — the [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp)
   command-line tool you install yourself.

Once both are in place, the render runs locally with control images derived from your
model, and nothing leaves your machine:

```
render a timber pavilion at golden hour, photoreal
```

> This is not fully one-click yet: you install the `sd` binary and download a model
> the first time. After that, `render <prompt>` just works, offline. For an accurate
> render of the real model (rather than a reimagined one), use the
> [ray tracer](05-raytrace.md) instead.

---

## You now have

A parametric structure you edited live — from both the Parameters tab and the
`paramset` verb — then froze to a final mesh, plus (optionally) a local, offline AI
render pipeline.
