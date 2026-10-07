---
name: design-references
description: "Use when building or reviewing UI, and the work risks looking generic: read the brief, then point at 77 named design references and say how to pick."
origin: bundled
---

# Design references

This skill exists because the default output of an agent asked to "make a nice
dashboard" is a purple gradient, a rounded card, and a three-word sans-serif
heading. The problem is not taste, it is that the model never looked at
anything. This skill hands it a fixed list of places to look, and a rule for
using them.

## The rule

Name the reference you are copying from, in the code, in a comment or a
commit message. A hero section built against Supahero says so. A motion
sequence lifted from Kinetics says so.

An unnamed reference is indistinguishable from the model's own defaults, which
means the next person cannot tell a decision from a habit.

## How to pick

Pick two or three references, not twelve. A page that draws from five
aesthetics is worse than a page that commits to one.

1. Name the artifact: a landing page, a nav, a hero, a table, a 404.
2. Find the section below for that artifact. Sections list sites that
   specialize in exactly that thing.
3. Open two or three of them. Actually open them. Do not work from memory of
   what a site probably looks like.
4. Copy structure, spacing rhythm, type scale, and interaction timing. Do not
   copy brand color, a logo, or a name.
5. Write down which references you used before writing the component.

## Read the room before drawing

The default failure is not bad taste, it is never reading the brief. Before
writing any component, state the read in one line: what kind of surface this
is, who it is for, and the language it should speak. "Terminal chat surface
for a developer reading logs at 2am, plain and dense, no decoration" is a
design decision. "Make it look nice" is not.

Three signals pick the reference more reliably than taste does:

- **Surface kind**: landing, portfolio, dashboard, table, docs, 404.
- **Audience**: an end user, a developer, or a procurement panel. Density
  and motion tolerance follow from this, not from preference.
- **Quiet constraints**: accessibility-first, regulated, public-sector,
  trust-first commerce. These override the aesthetic. A dense cockpit
  layout for a public service is wrong however good it looks.

When the brief genuinely diverges, ask one question, not a list. When it
can be inferred, do not ask at all.

## What to steal and what not to

Steal: the grid, the type scale and its ratios, the spacing scale, the hover
and focus states, the transition durations, the empty and error states.

Do not steal: the palette, the illustration set, the product name, the logo.
A page that reads as a reskin of the reference it copied is a failed port even
when the pixels match.

## When a component already exists

If the project already has a design system, the references are for the part
that is missing. New components extend the existing tokens. Do not introduce a
second button style because one of these galleries had a nicer hover.

## Anti-slop checklist

Before calling a surface done, check the ones that actually show up in agent
output:

- No gradient that exists only because gradients are the default.
- No three-word heading with a subtitle repeating the same three words.
- No card grid where every card has the same height and the same padding.
- No icon from one set next to an icon from another.
- No fake data that is obviously 123, 456, 789. Real-looking names, real
  lengths, one deliberately awkward long value so the layout is tested.
- Empty state, loading state, and error state exist and were looked at.
- Focus rings are visible. Keyboard tab order follows visual order.
- Contrast is checked, not assumed.
- No "AI-purple" gradient, dark mesh hero, or glassmorphism on surfaces
  that did not ask for one. These are model defaults, not choices.
- No Inter on slate-900 unless the brief named it. The default pairing is
  invisible, which means it was not made.
- Motion is on interaction, not looping forever on load.
- Nothing is centered because centering is the safe default, when the
  surface has a natural reading direction (left for text, right for data
  tables, top for dashboards).

## The registry

77 sites, grouped by what you are building. Sections are ordered roughly by
how often an agent task needs them.

### Component libraries (use when you need the thing itself)

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 1 | Balsa UI | https://balsa-ui.com | Minimal component set |
| 2 | Springs Studio | https://springs.studio | Component studio |
| 3 | Animos | https://animos.app | Animated components |
| 4 | Tegaki | https://gkurt.com/tegaki | Hand-drawn UI style |
| 5 | Meshfont | https://meshfont.com | Mesh-styled type UI |
| 6 | Spectrum UI | https://ui.spectrumhq.in | React component set |
| 7 | 21st.dev | https://21st.dev | Component registry with MCP and agent integration |
| 8 | shadcnblocks | https://shadcnblocks.com | Blocks built on shadcn |
| 9 | React Bits | https://reactbits.dev | React component patterns |
| 10 | 8bitcn | https://8bitcn.com | Retro 8-bit styled shadcn |
| 11 | Evil Charts | https://evilcharts.com | Charts, deliberately unconventional |
| 12 | Coss UI | https://coss.com/ui | Component set |
| 13 | Rare UI | https://rareui.com | Rare or unusual components |
| 14 | BeUI | https://beui.dev | Component set |
| 15 | Easy UI | https://easyui.site | Component set |
| 16 | Simply Buttons | https://simply-buttons.vercel.app | Buttons only |
| 17 | ReUI | https://reui.io | React and Tailwind components |
| 18 | shadcn/ui | https://ui.shadcn.com | The baseline everyone copies |
| 19 | Aceternity UI | https://ui.aceternity.com | Animated, showy components |
| 20 | Magic UI | https://magicui.design | Animated components |
| 21 | Motion Primitives | https://motion-primitives.com | Motion-focused components |
| 22 | Uiverse | https://uiverse.io | Community element library |
| 23 | UIAble | https://uiable.com | Component set |
| 24 | Vantaui | https://vantaui.com | Component set |
| 25 | MicroKit UI | https://microkit.co | Micro components |
| 26 | mapcn | https://mapcn.dev | Maps, markers, routes, popups |
| 27 | Liquid Glass | https://glass.samasante.com | Glass and refraction components |

### Design galleries (use when you need to see what good looks like)

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 28 | Minimal Gallery | https://minimal.gallery | Curated high-end websites |
| 29 | Kage | https://kage.design | UI inspiration mapped to prompts |
| 30 | Refero Styles | https://styles.refero.design | 2000+ real product styles and typography |
| 31 | Component Gallery | https://component.gallery | 2600+ component examples |
| 32 | AppShot Gallery | https://appshot.gallery | Real mobile app screenshots |
| 33 | Recent Design | https://recent.design | Latest design references |
| 34 | Curated Design | https://curated.design | Curated websites |
| 35 | Web Inspo | https://webinspoo.com | General inspiration |
| 36 | Landdding | https://landdding.com | Real design references |
| 37 | One Page Love | https://onepagelove.com/og | One-page sites |
| 38 | Saaspo | https://saaspo.com | SaaS websites |
| 39 | Landing Love | https://landing.love | Landing pages |
| 40 | SaaSFrame | https://saasframe.io | SaaS references |
| 41 | Open Design | https://open-design.ai | Design references |
| 42 | DesignMD | https://designmd.me | Design references |
| 43 | DESIGNmd Supply | https://designmd.supply | Design supply |
| 44 | DesignMD Hyperbrowser | https://design-md.hyperbrowser.ai | DesignMD hosted variant |
| 45 | DesignMD.ai | https://designmd.ai | Design systems in Markdown for agents |
| 46 | TypeUI | https://typeui.sh | Typography-driven UI |
| 47 | VibePrompt | https://vibeprompts.dev | Prompts for dashboards and landing pages |
| 48 | LogoToUse | https://logotouse.com | Logo inspiration |

### Specific UI pieces (use when the task is one page element)

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 49 | Navbar Gallery | https://navbar.gallery | Navbars |
| 50 | Supahero | https://supahero.io | Hero sections |
| 51 | 404s | https://404s.design | 404 pages |
| 52 | Footer Design | https://footer.design | Footers |
| 53 | CTA Gallery | https://cta.gallery | CTAs, forms, popups, buttons |
| 54 | Unsection | https://unsection.com | Page sections |
| 55 | 60FPS Design | https://60fps.design | Animation references |
| 56 | Design Spells | https://designspells.com | Microinteractions |
| 57 | Bento Grids | https://bentogrids.com | Bento grid layouts |
| 58 | Rebrand Gallery | https://rebrand.gallery | Rebrands |
| 59 | Gridddy | https://gridddy.framer.website | Grid layouts |
| 60 | Styles Refero | https://styles.refero.design | Product styles (dup of 30) |
| 61 | Mesh3D Gallery | https://mesh3d.gallery | 3D websites |
| 62 | MotionIn | https://motionin.design | Animation inspiration |

### Motion and animation (use when the page needs to move)

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 63 | ScrollTide | https://scrolltide.co | 200+ build prompts for 3D and scroll-driven sites |
| 64 | Kinetics | https://kinetics.colorion.co | 150+ motion effects with React code and prompts |
| 65 | Anime.js | https://animejs.com | Animation library, canonical docs |
| 66 | CSS Text Effects | https://text-effects.colorion.co | Text effects |
| 67 | Circle Loaders | https://circleloaders.dominikakissi.com | Loaders |
| 68 | Gradient Buttons | https://gradientbuttons.colorion.co | Gradient buttons |

### Illustrations and assets

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 69 | Kitbitz | https://kitbitz.art | 2000+ hand-drawn illustrations |
| 70 | 3Dicons | https://3dicons.co | 3D icon set |

### Typography and experimental

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 71 | Meshfont | https://meshfont.com | Mesh-styled type (dup of 5) |
| 72 | Tegaki | https://gkurt.com/tegaki | Hand-drawn type (dup of 4) |

### Agent-oriented design systems (use when the output must be machine-buildable)

| # | Site | URL | What it is good for |
|---|------|-----|---------------------|
| 73 | Aura | https://aura.build | Agent-facing design system |
| 74 | Neuform | https://neuform.ai | Agent-facing design system |
| 75 | DesignMD | https://designmd.me | Design systems in Markdown (dup of 42) |
| 76 | 21st.dev | https://21st.dev | Component registry (dup of 7) |
| 77 | shadcnblocks | https://shadcnblocks.com | Blocks (dup of 8) |

## Notes

- Entries 60, 71, 72, 75, 76, 77 duplicate earlier rows. They are kept so the
  count stays 77 and cross-referencing against other lists works.
- The MCP and agent integration on 21st.dev is the interesting one for this
  runtime: a component registry the agent can query, not just a page it reads.
- DesignMD.ai (45) publishes design systems as Markdown, which is the same
  idea as this skill: references as text the agent can actually load.
