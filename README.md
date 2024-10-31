# Tacho Bridge Application

The application is designed for use with the flespi platform. Communication with the server is organized through an MQTT channel 'tacho-bridge' that must be created in the user's account. Each card is should be represented in flespi as a separate device of type 'Tacho Bridge Card'.

## Specifications

Project uses [Tauri framework](https://tauri.app/) = [Rust](https://www.rust-lang.org/) + [Typescript](https://www.typescriptlang.org/) + [Vue 3](https://vuejs.org/) + [Quasar](https://quasar.dev/)

Quasar will be used as an interface, buttons, menu, etc. It was decided to abandon the native solution offered by Tauri due to possible difficulties with adaptation on different OS, and this will also facilitate the implementation of a mobile interface if required. Also, the native interface requires a bunch of imports that are already in Quasar.

## Getting started

Firstly it is needed to install [Rust](https://tauri.app/v1/guides/getting-started/prerequisites).

Init project from the root directory

```
npm install
```

Then it is needed fetch Cargo dependeces from the rust directory

```
cd src-tauri
cargo fetch
```

run project

```
npm run tauri dev
```

Build

```
npm run tauri build
```

Cargo can be updated only from the ./src-tauri directory

```
cargo update
```

## Icon generating & customizing

The project stores icons in a directory: **src-tauri/icons**

[Detailed description of icon generation and their characteristics from Tauri](https://tauri.app/v1/guides/features/icons/)

Tauri has a very convenient and super-simple tool for generating all the necessary icons for an application. What you need to do:

1. Upload a PNG image with a transparent background to the image directory "**src-tauri/icons**". The resolution should be **1024x1024**, this is the maximum icon size for MacOS, so that everything is displayed beautifully.
2. Run the Tool for generating icons from the <u>root of the project</u>:

```
npm run tauri icon src-tauri/icons/app-icon.png
```

**That's it. All the necessary icons of all sizes for all platforms will be generated.**

## Card connection states

<div>
    <div style="display: flex; align-items: center;">
        <img src="src/assets/credit_card_30dp_GREEN.svg" alt="Online" style="height: 30px; margin-right: 10px;">
        <span>Online connection to the server (OK)</span>
    </div>
    <div style="display: flex; align-items: center;">
        <img src="src/assets/credit_card_30dp_GRAY.svg" alt="Connected" style="height: 30px; margin-right: 10px;">
        <span>Physical connection to the computer (OK). <em>It is needed to check server address in the App config</em></span>
    </div>
    <div style="display: flex; align-items: center;">
        <img src="src/assets/credit_card_off_30dp_GRAY.svg" alt="Disconnected" style="height: 30px; margin-right: 10px;">
        <span>Has no physical connection to the computer and there is no connection to the server (Not OK). <em>Need to check everything :(</em></span>
    </div>
</div>
