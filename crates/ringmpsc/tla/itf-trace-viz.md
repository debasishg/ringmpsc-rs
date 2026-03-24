# Quint Trace Visualization

Options for visualizing simulation traces and invariant checking results from Quint.

## 1. ITF Trace Output (`--out-itf`)

Export the trace in **Informal Trace Format** (JSON) by adding `--out-itf` to your run command:

```bash
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --out-itf=trace.itf.json
```

This produces a JSON file containing the full sequence of states, suitable for downstream tools.

## 2. Quint Trace Explorer (Terminal UI)

An interactive TUI for navigating traces. Highlights state changes between steps and collapses unchanged sub-trees.

**Install via Nix:**
```bash
nix run github:informalsystems/quint-trace-explorer -- trace.itf.json
```

**Install via Cargo:**
```bash
git clone https://github.com/informalsystems/quint-trace-explorer.git
cd quint-trace-explorer && cargo build --release
./target/release/quint-trace-explorer trace.itf.json
```

**Key navigation:**

| Key | Action |
|-----|--------|
| `←` / `h` | Previous state |
| `→` / `l` | Next state |
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `Enter` / `→` | Expand node |
| `←` / `Backspace` | Collapse node |
| `d` | Toggle side-by-side diff view |
| `v` | Toggle variable visibility |
| `/` | Search/filter states |
| `g` | Go to state (prompts for number) |
| `q` / `Esc` | Quit |

## 3. ITF Trace Viewer (VS Code Extension)

Install the **ITF Trace Viewer** extension (`informal.itf-trace-viewer`) from the VS Code marketplace. It renders `.itf.json` files directly in the editor.

## 4. `--mbt` Flag (Model-Based Testing Metadata)

Adds two extra variables to each trace step:

- `mbt::actionTaken` — the action picked by the simulator at each step
- `mbt::nondetPicks` — a record of all nondeterministic values chosen

```bash
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --mbt
```

## 5. Verbosity Levels

Use `--verbosity=3` or `--verbosity=4` for detailed execution output on the console:

```bash
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant --verbosity=4
```

## Recommended Workflow for RingSPSC

```bash
# Generate ITF trace while checking invariant (with MBT metadata)
quint run RingSPSC.qnt --main=RingSPSC --invariant=safetyInvariant \
  --out-itf=trace.itf.json --mbt

# Explore interactively in the terminal
quint-trace-explorer trace.itf.json
```

Or open `trace.itf.json` in VS Code with the ITF Trace Viewer extension installed.
