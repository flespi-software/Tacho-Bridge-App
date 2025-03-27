# Tacho Bridge Application

![Tacho Bridge Application](src/assets/logo.svg 'Tacho Bridge Application')

The application is designed for use with the flespi platform. Communication with the server is organized through an MQTT channel 'tacho-bridge' that must be created in the user's account. Each card is should be represented in flespi as a separate device of type 'Tacho Bridge Card'.

## Download

You can always find the latest release here: [<kbd>↴ DOWNLOAD</kbd>](https://github.com/flespi-software/Tacho-Bridge-App/releases/latest)  
### MAC
- **tba_x.x.x_aarch64.dmg** _(Apple silicon machines)_
- **tba_x.x.x_x64.dmg** _(Intel-based machines)_

### Windows
- **tba_x.x.x_x64_en-US.msi** _(64-bit Windows machines)_

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
# default build command for the current OS. 
npm run tauri build 

# Build MacOS without signature and notarization.
npm run tauri build -- --target aarch64-apple-darwin    # targets Apple silicon machines.
npm run tauri build -- --target x86_64-apple-darwin     # targets Intel-based machines.
npm run tauri build -- --target universal-apple-darwin  # unversal app for x86 and ARM machines.
```

### MacOS code signing and notarization
Сreate a .env file with the variables described below with the specified credentials. IMPORTANT: this file is added to .gitignore, it will not be sent to the repository for the security purposes.
```
APPLE_IDENTITY="Developer ID Application: Your Name (YOUR_TEAM_ID)"
APPLE_TEAM_ID=YOUR_TEAM_ID
APPLE_ID=your.email@example.com
APPLE_PASSWORD=your-app-specific-password

# Enable notarization in Tauri 2.0
ENABLE_NOTARIZE=true
```
Then just run the *build-mac.sh* script which will check for the necessary variables, settings and start building a universal bundle that can run on all Mac architectures (x86 & ARM). The binary file will contain code for both architectures, the required one will be selected for launch.

If everything went well, you will see something like:
```
🔄 Restoring original configuration
✅ Build completed successfully!
📊 Application architecture information:
Architectures in the fat file: ./src-tauri/target/universal-apple-darwin/release/bundle/macos/tba.app/Contents/MacOS/tacho-bridge-application are: x86_64 arm64 

🏁 Script execution completed
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

![Online](src/assets/credit_card_30dp_GREEN.svg 'Online') Online connection to the server (OK)

![Connected](src/assets/credit_card_30dp_GRAY.svg 'Connected') Physical connection to the computer (OK). _It is needed to check server address in the App config_

![Disconnected](src/assets/credit_card_off_30dp_GRAY.svg 'Disconnected') Has no physical connection to the computer and there is no connection to the server (Not OK). _Need to check everything :(_

## License

[MIT](LICENSE) license.
