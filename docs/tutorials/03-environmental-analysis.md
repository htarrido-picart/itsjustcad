# Tutorial 3 — Environmental analysis

*You will georeference a site, place the sun, run a shadow study and a sun-hours
ground heatmap, read the report as numbers, and (optionally) have the deck
critique it. About 15 minutes.*

Every solar study in ItsJustCAD casts real rays from the real solar position
(NOAA SPA, computed in-repo) and is occlusion-accurate — buildings, terrain, and
even planted canopies block the sun like any other mesh.

---

## 1. Build something to shade, and set the location

A couple of masses to cast and receive shadows:

```
units m
box 8,8,0 10,24,26
name last tower
box 24,8,0 8,8,10
name last block
```

Georeference the document. `location` fixes the site on the globe and drives
every solar study; the third number is the timezone offset from UTC in hours:

```
location 40.71 -74.01 -5
```

(That is lower Manhattan, UTC−5.)

---

## 2. Place the sun at an instant

```
sun 40.71 -74.01 2024-12-21 12:00
```

`sun <lat> <lon> <date> <time>` lights the scene from the true solar position at
that moment. Switch to a perspective view and orbit to see the shading:

```
persp
ze
display shaded
```

---

## 3. Run a shadow study across a day

```
shadowstudy 2024-12-21 09:00 16:00 60
```

This sweeps the sun from 09:00 to 16:00 in 60-minute steps and draws the ground
shadow at each — the classic overlapping-shadow diagram for the winter solstice.

---

## 4. Run a sun-hours heatmap

Where the shadow study is a diagram, `sunhours` is a *measurement*: it casts
rays every 30 minutes across the date over a ground grid and colours each cell by
how many hours of direct sun it receives.

```
sunhours 2024-12-21 1.0
```

The second number is the grid spacing in metres (1 m here). Frame the top view
to read it as a heatmap:

```
top
ze
```

![A sun-hours ground heatmap — cool cells in shadow, warm cells in full sun](../screenshots/sun-hours.png)

---

## 5. Read the report — numbers, not vibes

The heatmap is the picture; the numbers are the point. Every study stores a
structured report:

```
report sunhours
```

You get the count of samples, min / average / max hours, a distribution, and the
worst and best sample locations — for example:

```
sunhours (2024-12-21, 1 m ground grid): 576 samples, min 0.0 / avg 1.5 / max 9.0 h
  distribution: <=1.5:324  <=3.0:102  <=4.5:97  <=6.0:37  <=7.5:4  <=9.0:12
  lowest:  0.0 h at (8.5, 8.5, 0.0) …
  highest: 9.0 h at (21.5, 9.5, 0.0) …
```

That is enough to say *"the courtyard on the north side gets under 1.5 h in
December — don't put the terrace there."*

---

## 6. Go further

- **Per-face insolation** on a specific object:
  `facesunhours tower 2024-12-21` — which facade gets the winter sun.
- **Annual radiation** from real weather, if you have an EPW file:
  `radiation tower /path/to/weather.epw` gives kWh/m²·yr per face.
- **A sun-path dome** for the whole year: `sunpath`.

Each of these stores its own report; `report` (bare) re-shows the most recent,
or name one: `report facesunhours`.

---

## 7. Ask the deck to critique it

If you have a backend configured (see [Tutorial 4](04-deck.md)), you don't have
to read the numbers alone. In the deck panel, type:

> *Read the sunhours report and tell me where not to put outdoor amenity space.*

The deck fetches the same structured `report` and grounds its critique in the
sampled values and locations — *"the strip along the north wall averages 0.5 h;
move the seating to the south-east corner where cells hit 9 h."* It reasons from
the measured numbers, not a guess.

---

## Notes

- Solar geometry is physically computed, but shadow and sun-hours studies are
  **design-stage analysis**, not certified daylighting or energy modelling.
- Exact usage for every verb here is in the
  [command reference](../command-reference.md):
  `location`, `sun`, `shadowstudy`, `sunhours`, `facesunhours`, `radiation`,
  `sunpath`, `report`.
