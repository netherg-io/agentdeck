app-title = Agentdeck

nav-instances = Инстансы
nav-drift = Расхождения
nav-plugins = Плагины
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
plugins-title = Плагины
processes-title = Процессы
processes-empty = Нет запущенных процессов агентов.
processes-stop = Остановить
processes-kind-agent = сессия
processes-kind-t3 = T3-скоуп
processes-kind-harness = раннер
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

plugins-empty = Нет доступных плагинов.
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
harness-note = Команда выполняется как показано, больше ничего не затрагивается.
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
pool-col-on = Вкл.
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

pool-search = Поиск моделей и инстансов
pool-all-instances = Все инстансы
pool-sort-instance = По инстансу
pool-sort-model = По модели
pool-sort-enabled = Сначала включённые
pool-no-match = Модели не найдены.
instances-rename = Переименовать
rename-save = Сохранить
