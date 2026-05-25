# Tyde Mobile Scaffold

This is intentionally only a launchable Phase 0 Tauri mobile shell. The
frontend is a minimal pairing/host shell, not the final product UI.

Useful commands from the repo root:

```sh
npm run mobile:dev
npm run mobile:ios:init
npm run mobile:ios:dev
```

`mobile:dev` launches the desktop-hosted Tauri shell from
`mobile-frontend/dist`. The iOS commands require the normal Tauri iOS
toolchain, including CocoaPods.

Mobile connectivity uses `mqtt-transport` over MQTT. The desktop host embeds
the effective broker endpoint in the QR payload (defaulting to
`mqtts://broker.emqx.io:8883` in current Tyde2); session encryption and
pairing are handled by the MQTT transport layer.
