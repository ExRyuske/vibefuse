# VibeFuse

Панель управления звуковой картой Arturia MiniFuse 1 для macOS, Windows и Linux.

[English](README.md)

- Настройки входа (INST, +48V, Direct monitor), индикаторы уровня и частота дискретизации.
- Микшер на три микса: Monitor & Phones, Mix 3/4 и Mix 5/6. На Windows он работает через
  драйвер Arturia, на macOS и Linux VibeFuse добавляет для него виртуальные устройства.
- Настройки возвращаются при каждом подключении карты.
- Пресеты, совместимые с MiniFuse Control Center.
- Работает из строки меню или трея. Русский и английский язык.

## Установка

Скачайте последнюю версию на странице Releases.

- **macOS 11 и новее.** Откройте `.dmg` и перетащите VibeFuse в «Программы». Программа не
  заверена Apple, поэтому в первый раз откройте её через правый щелчок → «Открыть».
- **Windows 10 или 11.** Сначала установите драйвер Arturia MiniFuse Audio Driver (он идёт
  вместе с MiniFuse Control Center). Распакуйте архив и запустите `VibeFuse.exe`.
- **Linux.** Распакуйте архив и запустите `./install.sh`. Он установит VibeFuse для вашего
  пользователя и добавит правило udev, поэтому один раз спросит `sudo`. Удаление —
  `./install.sh --remove`.

## Виртуальные выходы (macOS, Linux)

Устанавливаются в «Настройки → Виртуальные выходы». После этого программы видят устройства:

| Устройство | Для чего |
|---|---|
| VibeFuse Main 1/2 | основной звук |
| VibeFuse Virtual 3/4, 5/6, 7/8 | звук отдельных программ |
| VibeFuse Mix 3/4, Mix 5/6 | запись микса в программах для стримов и звонков |
| VibeFuse Loopback Main 7/8 | копия Monitor & Phones |

На macOS для этого нужен пароль администратора и доступ к микрофону: через него вход карты
попадает в миксы. На Linux нужен PipeWire 1.0 или новее.

## Сборка

```sh
cargo run --release
cargo test
```

Для Linux нужны `libfontconfig1-dev libfreetype-dev libxkbcommon-dev`, для macOS —
Xcode Command Line Tools. Пакет для выпуска под вашу систему собирает `sh scripts/package.sh`.

Чтобы выпустить версию, запустите в GitHub Actions workflow «Release» и выберите, какую
часть версии поднять. Он закоммитит новую версию, поставит тег и выложит релиз с пакетами
для macOS, Windows и Linux.

## Приватность

VibeFuse ничего не собирает. Пока открыто окно, раз в сутки он проверяет новую прошивку на
[странице MiniFuse у Arturia](https://www.arturia.com/products/audio/minifuse/resources).

## Лицензия

GPL-3.0 или более поздняя, см. [LICENSE](LICENSE). Лицензии компонентов лежат в каждом
пакете (THIRD-PARTY-NOTICES.txt).

VibeFuse не связан с Arturia. Arturia и MiniFuse — товарные знаки Arturia.
