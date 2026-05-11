# Tacho Bridge Application

![Tacho Bridge Application](src/assets/logo.svg 'Tacho Bridge Application')

The application is designed for use with the flespi platform. Communication with the server is organized through an MQTT channel 'tacho-bridge' that must be created in the user's account. Each card is should be represented in flespi as a separate device of type 'Tacho Bridge Card'.

## Download

You can always find the latest release here: [<kbd>↴ DOWNLOAD</kbd>](https://github.com/flespi-software/Tacho-Bridge-App/releases/latest)  
### MAC
- **tba_0.5.8_universal.dmg** _(All architectures machines)_

### Windows
- **tba_x.x.x_x64_en-US.msi** _(64-bit Windows machines)_

### Linux
- **tba_x.x.x_amd64.AppImage** _(64-bit Linux machines)_

## Specifications

Project uses [Tauri framework](https://tauri.app/) = [Rust](https://www.rust-lang.org/) + [Typescript](https://www.typescriptlang.org/) + [Vue 3](https://vuejs.org/) + [Quasar](https://quasar.dev/)

Quasar will be used as an interface, buttons, menu, etc. It was decided to abandon the native solution offered by Tauri due to possible difficulties with adaptation on different OS, and this will also facilitate the implementation of a mobile interface if required. Also, the native interface requires a bunch of imports that are already in Quasar.

## Getting started

Firstly it is needed to install [Rust](https://v2.tauri.app/start/prerequisites/).

Init project from the root directory

```
npm install
```


Cargo can be updated only from the ./src-tauri directory
```
cargo update
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

### Linux building & using
Minimum supported versions: **Ubuntu 22.04** / **Debian 12 (bookworm)** or newer.
Tauri v2 requires `libwebkit2gtk-4.1-dev` which is not available on older distributions.

To install system libraries you need *sudo* administrator rights. Please be careful when installing new packages and dependencies.

**[Tauri v2 Core Dependencies](https://v2.tauri.app/start/prerequisites/)**
```
sudo apt install -y build-essential curl wget pkg-config file \
  libssl-dev libxdo-dev libudev-dev \
  libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev
```
**PCSC Smart Card Support**
```
sudo apt install -y pcscd libpcsclite-dev libccid usbutils
```
**AppImage build dependencies**
```
sudo apt install -y squashfs-tools fuse
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

## Usage

TBA is a transparent APDU proxy between a remote Vehicle Unit (tachograph) and a physical tachograph card in a local smart card reader. The authentication ceremony is driven by the VU; TBA only relays bytes and extracts data that passes through.

### Linking a card

Before a card can be used, it must be linked in TBA:

1. Insert the card into a reader. TBA detects it and shows its **ICCID**.
2. Click the **Add Card** button (or use link mode on the detected card) to bind the physical card to a **card number** in the config.
3. Card numbers are 16 alphanumeric uppercase characters per Annex 1C regulation (EU 2016/799).

Without linking, an inserted card appears as "UNKNOWN CARD" and cannot authenticate.

### Card information display

Once linked and authenticated at least once, the reader block shows, under the reader name:

- **Name** — user-defined label (e.g. fleet identifier)
- **Card number** followed by `(generation | card type)` in grey, e.g. `4200000000525000 (Gen2 v2)`
- **Company name** and **company address** (icons 🏢 and 📍)
- **Expire** date, red if already past
- **Last auth** — last completed authentication time and its status (`success` green, `fail` red, `processing...` yellow during an active APDU exchange)

### Card data is only available during / after authentication

Tachograph cards protect their content with access conditions. Files like `EF_Identification` (card number, company name/address, expiry date) and `EF_Application_Identification` (card type, structure version) are readable **only** over Secure Messaging established by the Vehicle Unit during mutual authentication.

TBA does **not** hold any cryptographic keys (those are issued by EUR CA to certified VU/workshop manufacturers only), so it cannot originate SM commands on its own. Instead it passively observes the APDU stream passing through:

- VU issues `SELECT EF` + `READ BINARY` wrapped in SM after auth
- The card's response contains `DO'81` (plain value) + `DO'8E` (MAC) — tachograph SM protects integrity but **does not encrypt** the file content
- TBA parses the plaintext from `DO'81` and persists recognised fields

Currently recognised files:

| FID | File | Extracted fields |
|-----|------|------------------|
| `0520` | `EF_Identification` | cardExpiryDate → `expire`, companyName → `company_name`, companyAddress → `company_address` |
| `0501` | `EF_Application_Identification` | typeOfTachographCardId → `card_type`, cardStructureVersion → `structure_version` (keep highest on Gen2 cards that expose both Gen1 and Gen2) |

If the card has never been authenticated through TBA, these fields stay `null` in the config and the UI shows only the fields that are available pre-auth (ICCID, card number, name).

### Time storage and display

`last_auth` and `expire` are stored as UTC unix seconds. The UI renders them in the **local timezone of the machine**, so the same config file shown on computers in different regions displays the corresponding local time. Format: `YYYY-MM-DD HH:MM:SS`.

### Config and log location

Both files live in the user's Documents directory:

- **Config:** `~/Documents/tba/config.yaml` — server host, app identifier, per-card data (iccid, card number, expire, company info, last_auth, …)
- **Log:** `~/Documents/tba/log.txt` — rolling file log written by `fern`; contains APDU traces (at DEBUG), connection events (`[CONN] phase=… status=…`), and sniffer field extractions

On first run both are created automatically; if the OS denies write access, the UI surfaces a notification.

## License

[MIT](LICENSE) license.
