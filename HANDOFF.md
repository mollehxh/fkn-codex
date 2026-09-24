# Goal

## Исходная задача

Проект — Windows-сборка FKN Codex: локальный bridge, MCP-сервер, туннель и упаковка релиза. Пользователь хотел исследовать и исправить проблемы, из-за которых на Windows:

- локальный MCP bridge/туннель мог завершаться во время создания ChatGPT-коннектора;
- `tunnel-client.exe` мог игнорировать системный прокси Windows и получать от OpenAI ошибку `403 unsupported_country_region_territory`;
- connector creation мог показывать только общее `Something went wrong`;
- скрытый Codex-процесс мог завершиться, оставив локальный endpoint недоступным;
- MCP-сервер должен был передавать модели компактный контекст локального Codex (ОС, shell, cwd, режим доступа, computer-use), причём без раздувания MCP-инструкций;
- должна была появиться отдельная npm-команда, создающая готовый Windows ZIP, не ломая уже установленную/рабочую сборку;
- ошибка установки/listing системных skills с GitHub HTTP 401 должна была обрабатываться корректно.

Пользователь отдельно просил не ломать текущий установленный билд и не редактировать его вручную. Предпочтительный workflow — собрать отдельный архив через npm-команду и передать его другу.

## Ожидаемый конечный результат

После завершения работы должен быть получен новый Windows ZIP, который:

1. содержит актуальные `fkn-codex.exe`, `fkn-codex-bridge.exe`, `fkn-codex-auth-shim.exe`, `codex.exe` и `tunnel-client.exe`;
2. автоматически восстанавливает bridge/tunnel после преждевременного завершения;
3. учитывает системный WinINET/HTTPS/HTTP/ALL proxy для control-plane соединения туннеля;
4. оставляет MCP-запросы к localhost без проксирования;
5. содержит стабильный локальный endpoint во время connector discovery;
6. передаёт ограниченный локальный Codex context через MCP metadata и fallback в видимом tool description;
7. включает исправленный skill installer, который повторяет публичный GitHub-запрос анонимно после отклонения настроенного токена.

## Ограничения

- Уже установленный каталог в `C:\Program Files\fkn-codex-windows-x64-20260923` не изменять.
- Не удалять `target`, `dist` или существующие пользовательские изменения.
- Для изменений использовать `apply_patch`.
- Для Rust-кода после продолжения работы соблюдать инструкции из корневого `AGENTS.md`.
- Не считать `teach` официальным curated skill: в текущем `openai/skills` его нет. Ошибка для этого имени — отсутствие такого пути, а не Windows-поломка.

# What was done

## Диагностика проблем друга

Были разобраны две независимые причины старой ошибки connector creation:

1. `tunnel-client.exe` шёл к `api.openai.com` напрямую, хотя Chrome/Codex использовали системный прокси `127.0.0.1:10808`. OpenAI отвечал `403 unsupported_country_region_territory`. Установка `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` перед запуском tunnel client решала эту часть.
2. Локальный bridge на `127.0.0.1:8787` мог завершиться до повторных запросов ChatGPT (`502 failure_source: connect`, `rpc_method: server/discover`). Временная wrapper-схема друга держала порт 8787 и перезапускала настоящий bridge на 8789.

Это были реальные проблемы старой сборки/окружения, а не доказательство отсутствия доступа к репозиторию.

## Runtime resilience и proxy

В `fkn-bridge` добавлены:

- readiness endpoint `/readyz` с timeout и диагностическими ошибками;
- очистка Windows process tree через `taskkill.exe`;
- ожидание готовности bridge и tunnel при запуске;
- supervision/restart для bridge и tunnel с backoff;
- сохранение bridge при `pause`, отключение supervision только для tunnel;
- автоматический restart bridge при падении скрытого Codex;
- выбор control-plane proxy в порядке `FKN_TUNNEL_CONTROL_PLANE_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `HTTP_PROXY`, затем системный WinINET (`ProxyEnable`/`ProxyServer`);
- передача proxy только tunnel control plane, без проксирования локального MCP endpoint;
- cleanup процессов при ошибках запуска.

## MCP server instructions

Добавлен компактный bounded context локального Codex:

- ОС;
- shell;
- режим доступа (`read-only`, `workspace-write`, `danger-full-access`);
- статус computer use;
- ограниченный `cwd`.

Основной вариант передаётся через `InitializeResult.instructions`. Поскольку некоторые connector-клиенты это поле не показывают модели, тот же текст один раз зеркалируется в description стабильного инструмента `codex_skills_list`. Ограничения длины и отдельных полей заданы в коде, чтобы инструкции не разрастались.

## Skill installer

Проверено, что официальный curated каталог `openai/skills/skills/.curated` не содержит `teach`; `.experimental` также не дал такого пути. Поэтому установка `teach` по этому имени не должна считаться успешной или обходиться случайным `git clone`.

Найдена отдельная причина HTTP 401 при listing/install: настроенный `GITHUB_TOKEN`/`GH_TOKEN` мог быть просрочен или отклонён, после чего helper сразу падал. В `github_utils.py` добавлен безопасный retry того же публичного запроса без токена после `401`; корректный токен по-прежнему используется для первого запроса и приватных ресурсов.

## Packaging

Добавлена npm-команда `fkn:package:windows`, которая:

- собирает FKN Windows binaries через Cargo;
- находит repo release `codex.exe`, иначе установленный Codex;
- находит `tunnel-client.exe` из установки или `%USERPROFILE%\\.local\\bin`;
- создаёт timestamped каталог в `dist` и ZIP;
- не перезаписывает Program Files.

# Current state

## Что сейчас есть

- Исходные изменения FKN bridge, MCP context, skill installer и packaging находятся в рабочем дереве.
- Уже установленный Program Files билд не перезаписывался.
- В `dist` созданы два промежуточных ZIP:
  - `dist/fkn-codex-windows-x64-20260924-142331.zip`
  - `dist/fkn-codex-windows-x64-20260924-143203.zip`
- Оба архива содержат шесть файлов (`codex.exe`, три FKN exe, `tunnel-client.exe`, `README.txt`). Они включают актуальный bridge на момент упаковки, но используют старый установленный `codex.exe`; последний skill-installer fix в них ещё не попал.
- `dist/` игнорируется корневым `.gitignore`.

## Сборка и проверки

Успешно выполнено до остановки:

- `cargo clippy --manifest-path fkn-bridge/Cargo.toml --all-targets -- -D warnings`;
- `cargo test --manifest-path fkn-bridge/Cargo.toml`;
- `node fkn-bridge/direct-tools-smoke.mjs` после указания установленного Codex и пересборки debug bridge;
- Python unittest для skill installer: 3 теста passed;
- `just test -p codex-skills`: 51/51 passed;
- `just fix -p codex-skills`;
- `just fmt` в `codex-rs` и `cargo fmt` для `fkn-bridge`;
- `npx prettier --write` для package script и успешная проверка форматирования.

## Незавершённое / не проверено

Последняя попытка собрать новый release Codex:

```powershell
cd D:\dev\fkn-codex\codex-rs
cargo build -p codex-cli --bin codex --release
```

Сборка долго компилировала workspace, затем завершилась из-за нехватки памяти LLVM:

```text
rustc-LLVM ERROR: out of memory
Allocation failed
error: could not compile codex-app-server
error: could not compile codex-exec
error: could not compile codex-core
```

Оставшиеся процессы были остановлены Ctrl-C; итоговый exit code — `1`. Никаких исходников эта попытка не меняла. В `target` могут быть частичные артефакты; удалять их не нужно.

Из-за этого не создан новый финальный ZIP с последним `github_utils.py`. Текущий установленный `codex.exe` и два ранее созданных архива не содержат этот последний fix.

## Точная точка остановки

Остановились сразу после неудачной release-сборки из-за OOM. Реализацию продолжать в этом handoff не нужно; следующий агент должен сначала прочитать этот файл, проверить diff и только затем решить, повторять ли сборку с меньшей параллельностью.

# Changed files

## FKN bridge

- `fkn-bridge/src/bin/fkn-codex/runtime.rs` — runtime controller, readiness checks, Windows process cleanup, bridge/tunnel supervision, restart backoff, proxy discovery и передача proxy только control plane. Это главный файл исправлений Windows lifecycle.
- `fkn-bridge/src/bin/fkn-codex/main.rs` — вызов `RuntimeController::maintain(...)` в основном TUI loop, чтобы supervision реально выполнялся.
- `fkn-bridge/src/bin/fkn-codex/runtime_tests.rs` — новые тесты readiness и proxy normalization. Файл новый.
- `fkn-bridge/src/main.rs` — скрытый Codex больше не завершает весь bridge при падении: процесс перезапускается с backoff, endpoint остаётся стабильным.
- `fkn-bridge/src/server_context.rs` — новый bounded renderer локального Codex context, режимы доступа и лимиты размера.
- `fkn-bridge/src/lib.rs` — подключение `server_context`, генерация `ServerInfo.instructions`, зеркалирование инструкции один раз в `codex_skills_list.description` для клиентов, которые игнорируют `InitializeResult.instructions`.
- `fkn-bridge/src/lib_tests.rs` — проверки bounded server instructions и единственного fallback mirror в tool metadata.
- `fkn-bridge/direct-tools-smoke.mjs` — raw MCP smoke assertions для `InitializeResult.instructions` и mirrored tool description.

## Skill installer

- `codex-rs/skills/src/assets/samples/skill-installer/scripts/github_utils.py` — retry публичного GitHub request без токена после HTTP 401; закрытие `HTTPError`, чтобы не оставлять warning.
- `codex-rs/skills/tests/test_skill_installer.py` — тест, подтверждающий первый запрос с токеном и второй анонимный запрос после 401.

## Packaging

- `package.json` — добавлен script `fkn:package:windows`.
- `scripts/package-fkn-windows.mjs` — новый Windows-only packager, создающий timestamped `dist` directory и ZIP без изменения Program Files.

# Important implementation details

## Proxy behavior

Не выставлять глобальный proxy для bridge/MCP localhost. Proxy нужен именно для tunnel control-plane запросов. Приоритет настроек:

1. `FKN_TUNNEL_CONTROL_PLANE_PROXY`;
2. `HTTPS_PROXY`;
3. `ALL_PROXY`;
4. `HTTP_PROXY`;
5. Windows Internet Settings (`ProxyEnable` + `ProxyServer`).

## MCP context behavior

`InitializeResult.instructions` остаётся каноническим MCP-полем. Зеркалирование в `codex_skills_list` — fallback для ChatGPT connector behavior и не должно копироваться в descriptions всех инструментов. Сохранять жёсткие лимиты, чтобы не нарушить ограничения контекста.

## Skill behavior

HTTP 401 retry предназначен для публичных GitHub resources при неправильном локальном токене. Не логировать и не раскрывать значение токена. Для private resources анонимный retry закономерно может снова завершиться ошибкой.

`teach` не устанавливать обходным путём: сначала нужен реальный существующий repo/path или официальное появление skill в каталоге.

## Packaging continuation

После успешной малопараллельной release-сборки следующий шаг обычно такой:

```powershell
cd D:\dev\fkn-codex\codex-rs
cargo build -p codex-cli --bin codex --release -j 1

cd D:\dev\fkn-codex
npm run fkn:package:windows
```

`package-fkn-windows.mjs` предпочитает `codex-rs/target/release/codex.exe`, поэтому успешная release-сборка автоматически попадёт в новый архив и включит исправленный skill installer. Не удалять старые ZIP до проверки нового.

Если OOM повторится, сначала уменьшить параллельность (`-j 1` или эквивалентная переменная Cargo), а не менять исходники и не чистить весь target без необходимости.

## Что проверить в новой сессии

1. Прочитать этот файл и корневой `AGENTS.md`.
2. Выполнить только read-only `git status --short` и `git diff --stat`, чтобы подтвердить состояние.
3. Проверить, что последний `github_utils.py` и его тест действительно присутствуют.
4. При наличии ресурсов повторить release build с `-j 1`.
5. Запустить `npm run fkn:package:windows` и проверить содержимое нового ZIP.
6. Перед передачей другу не трогать Program Files; передать только новый ZIP и краткую инструкцию запуска.

# Handoff summary

Основная функциональность уже реализована и debug/FKN проверки прошли. Единственный незакрытый технический шаг — собрать новый release `codex.exe` с последним skill-installer fix и затем пересоздать отдельный ZIP. Последняя попытка release build остановилась не из-за ошибки кода, а из-за нехватки памяти LLVM. До этого установленный билд намеренно не изменялся.
