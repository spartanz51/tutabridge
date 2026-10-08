# TutaBridge GUI frontend

React + TypeScript frontend of the Tauri app (`src-tauri`). It talks to the
bridge through Tauri commands and events; all mail logic lives in Rust.

```bash
./dev.sh             # from the repo root: GUI in dev mode (cargo tauri dev)
npm run build        # production bundle in ui/dist (needed by tauri-build)
npx tsc --noEmit     # typecheck, as CI does
```

- `src/App.tsx`: header, status pill, tabs
- `src/components/`: Dashboard, Connection, Config, Backup panels
- `src/hooks/useBridge.ts`: status, stats and logs from the bridge
