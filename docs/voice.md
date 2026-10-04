# Русская озвучка Piper

**Piper 1.8.0 + `ru_RU-denis-medium`** — бесплатный локальный вариант русской нейросетевой озвучки для этой студии. Для работы достаточно CPU; платный API и GPU не нужны. Модель одного мужского голоса занимает около **63,2 МБ** и создаёт звук с частотой **22 050 Гц**. Это удобный старт для разговорного сценария; оцените тембр и произношение на своём тексте.

Настройку выполняйте из папки проекта при остановленном сервере. `doctor` и `demo` используют ту же папку данных, что и `serve`, поэтому эксклюзивная блокировка не допускает их одновременный запуск. Python-окружение и голос устанавливаются отдельно от Rust. Потребуется интернет для загрузки пакета и модели; сама озвучка работает локально.

## Windows / PowerShell

Установите 64-битный [Python](https://www.python.org/downloads/) версии 3.9 или новее. Команды ниже создают отдельное окружение; активировать его не нужно:

```powershell
py -3 -m venv .venv
.\.venv\Scripts\python.exe -m pip install piper-tts==1.8.0
.\.venv\Scripts\python.exe -m piper.download_voices ru_RU-denis-medium --data-dir data/voices

$studioRoot = (Get-Location).Path
$env:RUSTCLIP_PIPER = "$studioRoot\.venv\Scripts\piper.exe"
$env:PIPER_MODEL = "$studioRoot\data\voices\ru_RU-denis-medium.onnx"
$env:PIPER_CONFIG = "$studioRoot\data\voices\ru_RU-denis-medium.onnx.json"

cargo run --release -- doctor
cargo run --release -- demo
cargo run --release -- serve
```

Если Windows не распознаёт `py`, используйте полный путь к установленному `python.exe` в первой команде. FFmpeg, ffprobe и шрифт также нужны для рендера; их установка описана в [README](../README.md).

## Ubuntu / Debian / Linux

Установите Python с поддержкой виртуальных окружений, затем выполните:

```bash
sudo apt-get install -y python3-venv
python3 -m venv .venv
.venv/bin/python -m pip install piper-tts==1.8.0
.venv/bin/python -m piper.download_voices ru_RU-denis-medium --data-dir data/voices

export RUSTCLIP_PIPER="$(pwd)/.venv/bin/piper"
export PIPER_MODEL="$(pwd)/data/voices/ru_RU-denis-medium.onnx"
export PIPER_CONFIG="$(pwd)/data/voices/ru_RU-denis-medium.onnx.json"

cargo run --release -- doctor
cargo run --release -- demo
cargo run --release -- serve
```

Эта версия Piper и реальная генерация русской речи проверены на Linux CPU. Команды Windows используют опубликованный Windows-пакет; отдельная проверка Windows в CI пока отсутствует.

## Сохранить настройки и выбрать голос

Переменные из примеров действуют в текущем терминале. Для постоянной настройки перенесите **свои абсолютные пути** в `.env` по образцу [.env.example](../.env.example). На Windows можно использовать прямые слэши:

```dotenv
RUSTCLIP_PIPER='C:/projects/rustclip-studio/.venv/Scripts/piper.exe'
PIPER_MODEL='C:/projects/rustclip-studio/data/voices/ru_RU-denis-medium.onnx'
PIPER_CONFIG='C:/projects/rustclip-studio/data/voices/ru_RU-denis-medium.onnx.json'
```

На Linux путь к движку заканчивается на `.venv/bin/piper`. Конфигурацию `.onnx.json` берите **от той же модели**; скачиватель сохраняет её рядом. `PIPER_CONFIG` необязателен, если файл лежит рядом и называется `ru_RU-denis-medium.onnx.json`. Для автоматического выбора демо студия читает язык из этой конфигурации. Совместимость самой модели проверяется при рендере.

После изменения `.env` перезапустите сервер. В `doctor` должны быть доступны `piper` и `piper_model_available`, язык `piper_language` должен быть `ru`, а `preferred_voice` — `piper`. `demo` и «Демо +» используют подготовленный русский Piper; если он не подготовлен, выбирается eSpeak либо режим без голоса. Явный `demo --silent` отключает голос.

В своём проекте выберите **Piper** в поле голоса и выполните рендер заново. Настройка движка не заменяет ранее выбранный вами голос и не меняет уже созданный MP4. Если вы явно выбрали Piper, ошибка модели показывается как ошибка рендера; студия не подменяет её другой озвучкой.

Для ровной речи пишите короткие фразы, расставляйте знаки препинания и проверяйте имена, ударения и числа на слух. В рендере Piper применяется коэффициент громкости **0,85**, оставляющий запас при сведении; это не настройка интонации. Музыка и длительность сцен тоже влияют на разборчивость.

## Источники и лицензии

Лицензии относятся к разным компонентам:

| Компонент | Заявленная лицензия | Официальный источник |
|---|---|---|
| Движок Piper / пакет `piper-tts` 1.8.0 | GPL-3.0-or-later | [Репозиторий OHF](https://github.com/OHF-Voice/piper1-gpl), [версия пакета](https://pypi.org/project/piper-tts/1.8.0/) |
| Репозиторий `rhasspy/piper-voices` | MIT в метаданных репозитория | [README](https://huggingface.co/rhasspy/piper-voices/blob/main/README.md) |
| Датасет голоса Denis | CC0, указан в карточке конкретного голоса | [MODEL_CARD Denis](https://huggingface.co/rhasspy/piper-voices/blob/main/ru/ru_RU/denis/medium/MODEL_CARD), [датасет OHF](https://github.com/OHF-Voice/voice-datasets) |
| Код RustClip Studio | MIT | [LICENSE](../LICENSE) |

Лицензия движка и условия конкретного голоса проверяются отдельно. Не переносите условия Denis на другие голоса: у них могут быть ограничения, включая некоммерческое использование. При распространении самого движка соблюдайте его лицензию. Голос, Python-пакеты и модели не входят в исходники студии.

Официальная [инструкция Piper CLI](https://github.com/OHF-Voice/piper1-gpl/blob/main/docs/CLI.md) описывает установку и загрузку. Прямые файлы Denis: [модель `.onnx`](https://huggingface.co/rhasspy/piper-voices/resolve/main/ru/ru_RU/denis/medium/ru_RU-denis-medium.onnx), [конфигурация `.onnx.json`](https://huggingface.co/rhasspy/piper-voices/resolve/main/ru/ru_RU/denis/medium/ru_RU-denis-medium.onnx.json). SHA256 проверенной модели:

```text
15fab56e11a097858ee115545d0f697fc2a316c41a291a5362349fb870411b0a
```
