# Build and install taskpod as an Arch package, and its VS Code extension.
# `just` lists the recipes, `just install` does both.

vscode_dir := "editors/vscode"
vscode_version := `jq -r .version editors/vscode/package.json`
vsix := vscode_dir / "taskpod-" + vscode_version + ".vsix"

default:
    @just --list

# Build and install the Arch package and the VS Code extension
install: arch vscode

# Build the Arch package (dist/arch/PKGBUILD) and install it; pacman asks for sudo
arch:
    cd dist/arch && makepkg -sfi

# Build the Arch package without installing it
arch-build:
    cd dist/arch && makepkg -f

# Package the VS Code extension as a .vsix
vsix:
    cd {{ vscode_dir }} && bun install && bun run package

# Package the VS Code extension and install it into VS Code
vscode: vsix
    code --install-extension {{ vsix }} --force
