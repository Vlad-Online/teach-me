# teach-me на Mac: сборка и установка

Приложение собирается прямо на Mac (собрать его на Windows нельзя: нужны инструменты Apple).
Всё ниже — команды для приложения «Терминал». Время: ~30–40 минут, из них большая часть — первая компиляция.
Нужен Mac на Apple Silicon (M1 и новее) или Intel, macOS 11+.

## 1. Инструменты (один раз)

```bash
# Инструменты разработчика Apple (откроется окно установки — нажать «Установить»)
xcode-select --install

# Homebrew — менеджер пакетов (если ещё нет): https://brew.sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# ffmpeg (нарезка клипов), yt-dlp (скачивание с YouTube), node (нужен yt-dlp для YouTube)
brew install ffmpeg yt-dlp node

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Сборщик приложений Tauri
cargo install tauri-cli --version "^2" --locked
```

## 2. Сборка

Распакуйте архив с проектом (например, в `~/src/teach-me`), затем:

```bash
cd ~/src/teach-me
cargo build --release -p harvester                      # сборщик базы фраз
cd crates/app && cargo tauri build --bundles app,dmg    # приложение
```

Готовое приложение: `target/release/bundle/macos/teach-me.app`
(и установщик `target/release/bundle/dmg/teach-me_0.1.0_*.dmg`).
Перетащите `teach-me.app` в «Программы».

## 3. Настройки и ключи

При первом запуске приложение создаёт папку `~/teach-me` с файлами:
- `config.toml` — настройки и ключи API;
- `sources.toml` — список португальских YouTube-каналов для сборщика.

Откройте `~/teach-me/config.toml` (или в строке меню: значок teach-me → «Открыть папку с настройками»)
и впишите ключи:
- `[anthropic] api_key` — ключ Claude (console.anthropic.com): перевод фраз, проверка ответов;
- `[openai] api_key` — ключ OpenAI (platform.openai.com): распознавание речи.

После правки — перезапустить приложение (значок в строке меню → «Выход», затем открыть снова).

## 4. Первая база фраз

```bash
cd ~/teach-me
~/src/teach-me/target/release/harvester check-api     # проверить ключи
~/src/teach-me/target/release/harvester run -c 30     # найти видео и нарезать 30 фраз (~15 минут)
```

Клипы и база лежат в `~/teach-me/data`.

## 5. Как пользоваться

- Значок teach-me появляется в **строке меню** (вверху справа), в Доке его нет.
- Карточка всплывает раз в 25 минут (настраивается в `config.toml`, раздел `[trainer]`).
- **⌥ Option + Пробел** (держать) — ответ голосом; отпустить — отправить.
  На Mac `Ctrl+Пробел` занят переключением раскладки, поэтому другая клавиша.
- **⌃ Ctrl + ⌥ Option + P** — карточка прямо сейчас. Клик по значку — то же самое.

### Разрешения macOS

- **Микрофон** — macOS спросит при первом голосовом ответе. Если случайно запретили:
  «Системные настройки → Конфиденциальность и безопасность → Микрофон» → включить teach-me.
- **Входящие подключения** — при первом запуске macOS может спросить, разрешить ли teach-me принимать
  подключения: разрешить (нужно для синхронизации с телефоном).

## 6. Телефон (Android)

1. Установить на телефон teach-me (файл `.apk`).
2. На Mac: значок teach-me → «Подключить телефон…» — появится QR-код.
3. На телефоне: «Сканировать QR-код». Телефон и Mac должны быть в одной сети Wi-Fi.

## Если что-то не собирается

Mac-версия собрана по документации, но на настоящем Mac ещё не проверялась.
Если сборка падает, проще всего запустить на этом Mac Claude Code в папке проекта
и попросить: «собери teach-me под macOS по docs/MACOS.md и исправь ошибки».
