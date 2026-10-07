---
title: Laptop setup
type: howto
summary: "Setting up a dev laptop: winget package list, WSL Ubuntu 24.04, dotfiles samriv/dotfiles, Node via fnm (since 2026-09-25), Python via uv."
tags: ["laptop","setup","windows","wsl"]
created: 2026-09-09T19:00:00-05:00
updated: 2026-09-25T21:15:00-05:00
updated_by: curator
---

# Laptop setup

1. Install the apps with `winget import -i C:/Projects/dotfiles/winget.json`.
2. Install WSL with `wsl --install -d Ubuntu-24.04`.
3. Clone `samriv/dotfiles` into `C:/Projects/dotfiles` and run `./install.ps1`.
4. Node: install `fnm` (`winget install Schniz.fnm`), then `fnm install 24`. (Switched from nvm-windows on 2026-09-25: fnm is faster and works the same in WSL.)
5. Python: install `uv` and let each project pin its version (`uv python pin 3.13`).
6. Git signs commits with the SSH key from [[ssh-keys]].
