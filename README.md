# 🩸 BlueVein
BlueVein — кроссплатформенный инструмент на Rust для синхронизации ключей сопряжения Bluetooth между Linux и Windows на устройствах с двойной загрузкой.  

> [!warning] 
> Утилита находится в разработке.
> На данный момент она не работает!

# 📋 Описание
В системах с двойной загрузкой (dual boot) Linux и Windows используют разные форматы хранения ключей сопряжения Bluetooth-устройств. Это приводит к проблеме: если вы подключаетесь к Bluetooth-устройству в одной ОС, то при переключении на другую ОС устройство не подключится автоматически. Чтобы восстановить подключение, приходится удалять устройство из списка и повторно его добавлять.

BlueVein решает эту задачу, синхронизируя ключи сопряжения между Linux и Windows. Программа централизованно хранит и обновляет ключи в общем JSON-файле на EFI-разделе, обеспечивая совместимость форматов и бесшовное переключение между системами без необходимости повторного сопряжения.

# ✨ Основные возможности
- 🔍 Автоматическое обнаружение Bluetooth-адаптера и сопряжённых устройств.
- 🔑 Сбор и обновление ключей сопряжения (link keys) в JSON-файл на EFI-разделе.
- 📡 Мониторинг событий Bluetooth через D-Bus (Linux) и реестр Windows.
- 💻 Кроссплатформенная работа на Linux и Windows.
- ⚙️ Запуск как сервис (systemd на Linux, служба Windows) или вручную из терминала.
- 🔄 Обеспечение бесшовной работы Bluetooth-устройств при переключении между ОС.


# 🚀 Установка и запуск
## 🐧 Linux
1. Сборка:
   ```bash
   make linux
   ```
2. Запуск вручную:
   ```bash
   sudo ./target/release/bluevein-linux
   ```

3. Рекомендуется настроить systemd-сервис для автозапуска и управления.
    ```bash
    make install-linux 
    ```

## 🪟 Windows
1. Сборка:
   ```powershell
   make windows
   ```

2. Запуск с правами администратора:
   ```powershell
   .\target\release\bluevein-windows.exe
   ```

3. Можно установить как службу Windows:
   ```powershell
   .\bluevein-windows.exe install
   Start-Service bluevein-windows
   ```

# ⚙️ Конфигурация
- 📄 Конфигурационный файл Linux по умолчанию: `/etc/bluevein.conf`
- 📂 Путь к EFI-разделу определяется автоматически, но может быть изменён в конфиге.
- 🔧 Windows автоматически определяет EFI-раздел и работает с реестром.


# 🗂️ Структура файла ключей
Файл `bt_keys.json` хранится в EFI-разделе и содержит:
```json
{
  "adapter_mac": "XX:XX:XX:XX:XX:XX",
  "devices": {
    "YY:YY:YY:YY:YY:YY": "LINK_KEY_HEX_STRING",
    ...
  }
}
```

# 🤝 Вклад и развитие
Проект открыт для предложений и улучшений.  
Если у вас есть идеи, баги или вопросы — создавайте Issue или PR.

# 📄 Лицензия
Проект распространяется под лицензией [GPLv3](./LICENSE).

