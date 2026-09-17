#!/bin/bash
# filepath: /Users/shev/Projects/flespi/github/Tacho-Bridge-App/load-env.sh

# Clear output for better readability
clear

# Display information message
echo "🔄 Loading environment variables from .env file..."

# Check if .env file exists
if [ ! -f .env ]; then
    echo "❌ Error: .env file not found!"
    exit 1
fi

# Counter for loaded variables
count=0

# Load all lines from the .env file
while IFS= read -r line || [ -n "$line" ]; do
    # Skip comments and empty lines
    if [[ ! "$line" =~ ^// ]] && [[ ! "$line" =~ ^# ]] && [ -n "$line" ]; then
        # Split the line into variable name and value
        var_name=$(echo "$line" | cut -d= -f1)
        var_value=$(echo "$line" | cut -d= -f2-)
        
        # Remove quotes from value if present
        var_value=$(echo "$var_value" | sed 's/^"//;s/"$//')
        
        # Export the variable
        export "$var_name"="$var_value"
        count=$((count+1))
    fi
done < .env

# Display success message
echo "✅ Successfully loaded $count variables"

# List loaded variables
echo ""
echo "📋 Loaded variables:"
if [ -n "$APPLE_IDENTITY" ]; then
    # Mask part of the value for security
    masked_identity="${APPLE_IDENTITY:0:20}...${APPLE_IDENTITY: -10}"
    echo "   • APPLE_IDENTITY: $masked_identity"
else
    echo "   • APPLE_IDENTITY: ❌ not set"
fi

if [ -n "$APPLE_TEAM_ID" ]; then
    echo "   • APPLE_TEAM_ID: $APPLE_TEAM_ID"
else
    echo "   • APPLE_TEAM_ID: ❌ not set"
fi

if [ -n "$APPLE_ID" ]; then
    # Mask email for security
    local_part=$(echo "$APPLE_ID" | cut -d@ -f1)
    domain=$(echo "$APPLE_ID" | cut -d@ -f2)
    masked_email="${local_part:0:3}...@$domain"
    echo "   • APPLE_ID: $masked_email"
else
    echo "   • APPLE_ID: ❌ not set"
fi

if [ -n "$APPLE_PASSWORD" ]; then
    echo "   • APPLE_PASSWORD: [hidden]"
else
    echo "   • APPLE_PASSWORD: ❌ not set"
fi

if [ "$ENABLE_NOTARIZE" = "true" ]; then
    echo "   • ENABLE_NOTARIZE: $ENABLE_NOTARIZE (notarization enabled)"
else
    echo "   • ENABLE_NOTARIZE: $ENABLE_NOTARIZE (notarization disabled)"
fi

echo ""

# Modify the Tauri configuration file temporarily
TAURI_CONFIG="./src-tauri/tauri.conf.json"
TAURI_CONFIG_BACKUP="${TAURI_CONFIG}.bak"

# Create a backup of the original config
cp "$TAURI_CONFIG" "$TAURI_CONFIG_BACKUP"

# Set universal architecture
echo "🔧 Setting up for universal build (Intel + Apple Silicon)"
export TAURI_ARCH="universal"

# Replace environment variable placeholders with actual values
if [ -n "$APPLE_IDENTITY" ] && [ -n "$APPLE_TEAM_ID" ]; then
    echo "🔧 Updating Tauri configuration with actual values"
    
    # Read the JSON content
    CONFIG_CONTENT=$(cat "$TAURI_CONFIG")
    
    # Replace environment variables with actual values
    CONFIG_CONTENT=${CONFIG_CONTENT//@env:APPLE_IDENTITY/$APPLE_IDENTITY}
    CONFIG_CONTENT=${CONFIG_CONTENT//@env:APPLE_TEAM_ID/$APPLE_TEAM_ID}
    
    # Add targets field to macOS section if it doesn't exist
    if [[ ! "$CONFIG_CONTENT" =~ "\"targets\":" ]]; then
        CONFIG_CONTENT=$(echo "$CONFIG_CONTENT" | sed 's/"macOS": {/"macOS": {\n      "targets": ["x86_64-apple-darwin", "aarch64-apple-darwin"],/')
    fi
    
    # Write the updated config back
    echo "$CONFIG_CONTENT" > "$TAURI_CONFIG"
    echo "✅ Updated configuration for universal build"
    
else
    echo "⚠️ Signing credentials not found, proceeding with build without code signing"
    export TAURI_SIGNING_SKIP=true
fi

# Updater signing key: with bundle.createUpdaterArtifacts enabled the bundler
# must sign the .app.tar.gz updater artifact, otherwise the build fails.
# The key is generated once via `npx tauri signer generate` (same key that CI
# holds in the TAURI_SIGNING_PRIVATE_KEY secret).
if [ -z "$TAURI_SIGNING_PRIVATE_KEY" ]; then
    UPDATER_KEY_FILE="$HOME/.tauri/tba-updater.key"
    if [ -f "$UPDATER_KEY_FILE" ]; then
        export TAURI_SIGNING_PRIVATE_KEY="$UPDATER_KEY_FILE"
        export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
        echo "   • TAURI_SIGNING_PRIVATE_KEY: $UPDATER_KEY_FILE"
    else
        echo "❌ Updater signing key not found ($UPDATER_KEY_FILE) — the build will fail."
        echo "   Generate it with: npx tauri signer generate -w ~/.tauri/tba-updater.key"
    fi
fi

# Run the build with universal architecture
echo "🚀 Starting universal Tauri build (Intel + Apple Silicon)..."
npm run tauri build -- --target universal-apple-darwin
BUILD_RESULT=$?

# Restore the original config
echo "🔄 Restoring original configuration"
mv "$TAURI_CONFIG_BACKUP" "$TAURI_CONFIG"

if [ $BUILD_RESULT -eq 0 ]; then
    echo "✅ Build completed successfully!"
    
    # Check the architecture of the built app
    APP_PATH="./src-tauri/target/universal-apple-darwin/release/bundle/macos/tba.app"
    BINARY_PATH="$APP_PATH/Contents/MacOS/tacho-bridge-application"
    
    if [ -f "$BINARY_PATH" ]; then
        echo "📊 Application architecture information:"
        lipo -info "$BINARY_PATH"
    else
        echo "⚠️ Cannot verify architecture - binary not found at expected location"
        find ./src-tauri/target -name "tba.app" -type d | while read -r app; do
            echo "🔍 Found app bundle at: $app"
            lipo -info "$app/Contents/MacOS/tacho-bridge-application" 2>/dev/null || echo "  Cannot read architecture info"
        done
    fi
else
    echo "❌ Build failed with error code $BUILD_RESULT"
fi

echo ""
echo "🏁 Script execution completed"