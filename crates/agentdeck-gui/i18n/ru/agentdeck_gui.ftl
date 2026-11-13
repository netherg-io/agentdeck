app-title = Agentdeck

nav-instances = Инстансы
nav-drift = Расхождения
nav-processes = Процессы

loading = Загрузка…
load-error = Не удалось загрузить: { $what }

instances-title = Инстансы
instances-loading = Загрузка…
instances-empty = В реестре пока нет инстансов.
instances-add = Добавить
instances-remove = Удалить
instances-quotas = Квоты
instances-standalone = Другие источники расхода
quotas-no-data = Данных об использовании пока нет.
status-enabled = включён
status-disabled = выключен
t3-label = T3
t3-unavailable = T3 недоступен
threads-count =
    { $count ->
        [one] { $count } тред
        [few] { $count } треда
        [many] { $count } тредов
       *[other] { $count } тредов
    }

account-ok = ок
account-stale = устаревшие
account-error = ошибка
account-waiting = ожидание
quota-in-days = сброс через { $d } д { $h } ч
quota-in-hours = сброс через { $h } ч { $m } мин
quota-in-mins = сброс через { $m } мин

drift-title = Расхождения
drift-no-drift = Расхождений нет.
drift-apply = Применить
drift-import = Импортировать
drift-import-server = Импортировать { $server }
drift-fix = Исправить (сначала бэкап)
drift-reenable = Переподключить
drift-confirm = Подтвердить
drift-confirm-replace = Подтвердить замену { $path }
drift-cancel = Отмена
drift-preview = Предпросмотр
drift-working = Выполняется…
drift-group-t3 = Инстансы провайдера T3
drift-not-in-registry = Нет в реестре.
processes-title = Процессы
processes-empty = Нет запущенных процессов агентов.
processes-stop = Остановить
processes-kind-agent = сессия
processes-kind-t3 = T3-скоуп
processes-kind-harness = раннер
processes-kind-container = podman
processes-confirm-title = Остановить { $title }?
processes-confirm-will = Будут остановлены эти процессы:
processes-force = Принудительно завершить (SIGKILL) после 5 с ожидания
processes-confirm-ok = Остановить
processes-cancel = Отмена
processes-stop-terminated = остановлены
processes-stop-killed = принудительно остановлены
processes-stop-exited = уже завершены
processes-stop-running = всё ещё работают после ожидания
processes-stop-result = { $title }: { $what }
coming-soon = Появится в следующих юнитах.

plugins-confirm = Подтвердить
plugins-cancel = Отмена
plugins-enabled = включён
plugins-disabled = выключен
plugins-unavailable = недоступен (нужен T3)
plugins-preview-enable = Включение { $name } выполнит:
plugins-preview-disable = Выключение { $name } уберёт его файлы (данные сохранятся):
add-title = Новый инстанс
add-back = Назад к инстансам
add-harness = Харнесс
add-id = ID инстанса
add-id-note = ID нельзя переименовать: к ним привязана история T3.
add-display-name = Отображаемое имя
add-home = Домашний каталог
add-home-use-suggestion = Подставить
add-preview = Предпросмотр
add-apply = Применить
add-working = Работаю…
add-login-title = Вход
add-login-start = Начать вход
add-login-device-auth = Вход по коду устройства (codex)
add-login-code = Вставленный код
add-login-send = Отправить
add-login-cancel = Отмена
add-login-copy = Копировать
add-login-open = Открыть
add-login-cancelled = Вход отменён.
add-login-done = Процесс входа завершился с кодом 0.
add-login-failed = Ошибка входа: { $reason }.
add-login-retry-device = Порт 1455 занят; повторить через код устройства
add-login-device-code = Код устройства: { $code }
add-open-failed = Не удалось открыть браузер; скопируйте ссылку.
add-wrapper-title = Обёртка терминала (необязательно)
add-t3-title = Синхронизация с T3 (необязательно)

remove-ask = Удалить { $id }?
remove-confirm = Удалить
remove-cancel = Оставить
remove-keep-home = Каталог входа сохраняется.
remove-pool = Из пула уберутся: { $models }
remove-usage = Из квот уберутся: { $sources }
remove-failed = Не удалось удалить: { $what }
remove-threads =
    { $count ->
        [one] У T3 остался { $count } тред этого id; история сохранится, но не запустится.
        [few] У T3 осталось { $count } треда этого id; история сохранится, но не запустится.
        [many] У T3 осталось { $count } тредов этого id; история сохранится, но не запустится.
       *[other] У T3 осталось { $count } тредов этого id; история сохранится, но не запустится.
    }

nav-pool = Пул
nav-shared = Общее
nav-harnesses = Харнессы
action-apply = Применить
action-cancel = Отмена
action-working = Выполняется…
action-none = Менять нечего: всё уже синхронизировано.

add-login-agy = У agy нет CLI для входа: войдите аккаунтом Google в Antigravity IDE. Это один аккаунт на пользователя ОС.

pool-title = Пул
pool-empty = В пуле пока нет записей.
pool-add-title = Добавить запись
pool-instance = Инстанс
pool-model = Модель
pool-no-models = Для этого инстанса нет кеша моделей: введите id модели.
pool-options = Опции (k=v через запятую)
pool-preview-add = Предпросмотр
pool-preview-title = Предпросмотр изменения пула
pool-warning = Предупреждение: { $what }

shared-title = Общее
shared-skills = Навыки
shared-skills-sync = Синхронизировать навыки
shared-instr = Файлы инструкций
shared-instr-link = Связать файлы инструкций
shared-mcp = MCP-серверы
shared-mcp-sync = Синхронизировать MCP
shared-mcp-hidden = Значения серверов скрыты.
shared-mcp-none = Канонических MCP-серверов не найдено.
shared-preview-title = Предпросмотр изменений

harness-title = Харнессы
harness-not-installed = не установлен
harness-update = Обновить
harness-install = Установить
harness-run-title = Выполнить эту команду?
harness-result = { $name }: { $what }

nav-group-accounts = Аккаунты
nav-group-system = Система
processes-filter = Фильтр по имени или команде
processes-no-match = Ни одна группа не подходит под фильтр.
processes-count = { $count ->
    [one] { $count } процесс
    [few] { $count } процесса
   *[many] { $count } процессов
}
remove-done = { $id } удалён.

drift-loading = Проверяю расхождения…
drift-all-clear = Всё в порядке: расхождений нет.
pool-add = Добавить
pool-col-options = Опции
pool-col-on = Роутинг
plugins-details = Технический статус
plugins-no-details = Нет подробностей.
shared-ok-count = в порядке: { $count }
shared-todo-count = к синхронизации: { $count }
shared-skills-none = Каталоги навыков не найдены.
shared-instr-none = Файлы инструкций не найдены.
shared-mcp-servers = Канонические серверы
shared-mcp-targets = Цели
add-step-details = Данные
add-step-preview = Проверка
add-step-login = Вход
add-step-finish = Готово
add-next = Далее
add-edit = Назад
add-done = Готово
add-added = Добавлен { $id }.

pool-search = Поиск
pool-all-instances = Все инстансы
pool-sort-instance = По инстансу
pool-sort-model = По модели
pool-sort-enabled = Сначала включённые
pool-no-match = Модели не найдены.
instances-rename = Переименовать
rename-save = Сохранить

instances-cli = CLI
cli-open-error = Не удалось открыть CLI: { $what }

parallel-label = Параллельные задачи
parallel-unlimited = Без ограничений
parallel-invalid = Введите целое число от 1 до 4294967295 или оставьте поле пустым для снятия ограничения.
nav-group-usage = Квоты
nav-group-router = Роутер
plugin-imagegen = Генерация изображений
plugin-mcp-server = MCP agentdeck
nav-group-imagegen = Изображения
nav-imagegen-settings = Настройки
imagegen-provider = Шлюз
imagegen-model = Модель
imagegen-model-hint = Для всех картинок, кроме прозрачных. Пусто — модель шлюза по умолчанию.
imagegen-transparent-model = Модель для прозрачного фона
imagegen-transparent-hint = Модель изображений Vercel с альфа-каналом, для --transparent. Пусто — по умолчанию.
imagegen-key-set = Ключ задан. Оставьте пустым, чтобы не менять. Файл ключа общий с решениями роутера.
imagegen-key-missing = Нет ключа: генерация не будет работать, пока его не задать.
imagegen-check-ok = { $model } и { $transparent } доступны
imagegen-check-missing = { $model } нет на этом шлюзе
imagegen-check-not-image = { $model } не генерирует изображения
imagegen-check-transparent = { $model } — не модель изображений Vercel: прозрачные картинки не получатся
nav-group-agents = Агенты
nav-usage-sources = Источники
nav-usage-widget = Панель
nav-limits = Лимиты
usage-sources-title = Источники квот
usage-widget-title = Виджет на панели
limits-title = Параллельные задачи
usage-add = Добавить источник
usage-add-title = Новый источник квот
usage-preview-add = Предпросмотр
usage-preview-title = Изменения источников квот
usage-kind = Провайдер
usage-kind-missing = Выберите провайдера.
usage-id = ID
usage-display = Отображаемое имя
usage-api-key-env = Переменная с API-ключом
usage-db-file = База OpenCode
usage-optional = { $what } (необязательно)
usage-instance = Инстанс
usage-instance-none = Не привязан
usage-empty = Нет источников квот.
usage-widget-scope = Скрытые провайдеры пропадают только с панели COSMIC; опрос и роутинг их по-прежнему используют.
usage-order-title = Порядок на панели
usage-sources-count = { $count ->
        [one] { $count } источник
        [few] { $count } источника
        [many] { $count } источников
       *[other] { $count } источников
    }
sync-auto-apply = Автоприменение

nav-usage-poll = Опрос
nav-router-settings = Решения
nav-modules = Модули
usage-poll-title = Опрос квот
router-settings-title = Решения роутера
modules-title = Модули
settings-save = Сохранить
settings-saved = Сохранено
settings-seconds-invalid = Введите целое число секунд или оставьте пустым для значения по умолчанию
settings-decision-model = Модель для решений
settings-decision-model-hint = Модель, которая выбирает исполнителя для каждой делегированной задачи. Пусто — по умолчанию.
settings-key-placeholder = Вставьте новый ключ для замены
settings-key-set = Ключ задан. Оставьте пустым, чтобы сохранить его.
settings-key-missing = Ключа нет: роутинг не работает, пока его не задать.
settings-refresh = Интервал опроса, с
settings-refresh-hint = Не меньше { $min } с. Применяется со следующего цикла демона.
settings-stale = Устаревают через, с
settings-stale-hint = Кэшированные данные квот старше этого помечаются устаревшими.
pool-remove-tip = Удалить свою запись
usage-api-key = API-ключ
usage-api-key-placeholder = Пусто, если переменная уже задана
usage-key-write = Записать { $var } в { $path } (0600)
t3-ready = T3 готов
t3-disabled = T3 выключен
settings-decision-provider = Провайдер решений
settings-decision-model-vercel = Id модели в шлюзе, например openai/gpt-5-mini. Обязательно.
settings-model-required = Для Vercel AI Gateway нужна модель решений
settings-key = Ключ API { $provider }
pool-show-legacy = Устаревшие ({ $count })
settings-test = Проверить
settings-testing = Проверка…
settings-test-ok = { $model } работает: объём { $size }, сложность { $difficulty }, { $ms } мс{ $cost }
settings-rules = Правила моделей
settings-rules-source-default = Встроенные значения
settings-rules-source-user = Ваши сохранённые правила
settings-rules-placeholder = regex через запятую
settings-no-review = Модели без ревью
settings-no-review-hint = Regex через запятую; запятую внутри выражения экранируйте \,. Пусто — запрет выключен.
settings-deny = Запрещённые модели
settings-deny-hint = Список regex через запятую. Не выбираются автоматически.
settings-rules-note = Явно названную модель всё равно можно использовать.
settings-eval-title = История оценок
settings-eval-note = Только наблюдения; роутинг их не использует.
settings-eval-missing = Файла оценок пока нет; ничего не записано.
settings-eval-empty = Оценок пока нет.
settings-eval-unreadable = Не удалось прочитать оценки: { $what }
settings-eval-corrupt = Файл оценок повреждён в строке { $line }: { $what }. Файл оставлен без изменений.
settings-eval-refresh = Обновить
settings-eval-self = самопроверка
settings-eval-self-unknown = самопроверка неизвестна
settings-eval-cost = цена ${ $cost }
settings-eval-cost-unknown = цена не указана
settings-eval-evidence-ids = { $count } id свидетельств
settings-eval-judge = судья { $who }
settings-eval-confidence = уверенность { $value }
settings-eval-workflow = workflow { $run }/{ $step }

settings-eval-minutes-ago = { $count } мин назад
settings-eval-hours-ago = { $count } ч назад
settings-eval-days-ago = { $count } дн назад

drift-instr-use-shared = Использовать общий
drift-instr-use-local = Сделать общим
drift-instr-link-consequence = { $path } будет связан с ~/AGENTS.md.
drift-instr-import-consequence = Эта версия заменит ~/AGENTS.md и обновит все связанные с ним файлы.
drift-instr-backup = Резервная копия сохранится в { $path }
drift-instr-resolved = Файл инструкций согласован.
drift-instr-compare = Сравнение инструкций
drift-instr-confirm-shared = Связать с общим
drift-instr-confirm-local = Заменить общий
