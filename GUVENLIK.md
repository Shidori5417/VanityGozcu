# Güvenlik ve risk sınırları

## Token yönetimi

- Token'lar sadece ortam değişkeninden okunur, `secrecy::SecretString` içinde tutulur.
- `Debug`/`Display` çıktılarında ve loglarda token asla görünmez; HTTP hata gövdeleri loglanmadan önce `Authorization` benzeri alanlar maskelenir.
- Webhook URL'si de gizli sayılır (içinde token var); loglarda yalnızca son 4 karakter gösterilir.
- Config dosyasında token benzeri bir değer bulunursa program başlamaz.
- SQLite veritabanına token yazılmaz.

## Otomatik alma riski

- Kullanıcı token'ıyla API kullanımı Discord ToS'a aykırıdır; hesap kapatılabilir. Mod varsayılan olarak kapalıdır ve TUI'den açarken onay penceresi bu riski gösterir.
- Ana hesabın yerine bu riski kabul ettiğin ayrı bir hesap tercih et (o hesabın hedef sunucuda `MANAGE_GUILD` yetkisi olmalı).
- Araç şunları **yapmaz**: MFA/captcha atlatma, çoklu token rotasyonu, proxy ile rate limit atlatma, başka sunuculara/hesaplara erişim.
- Doğrulama istenirse deneme durur ve acil bildirim gönderilir; URL'yi elle almak sana kalır.

## Ağ davranışı

- Discord'un rate limit başlıklarına tam uyum; 429 sonrası global bekleme.
- Ardışık hatalarda üstel geri çekilme (exponential backoff), üst sınır `backoff_max_secs`.
- Makul `User-Agent`, tek HTTP client.

## Sistem

- systemd altında ayrı, yetkisiz kullanıcıyla çalışır (`YAPILANDIRMA.md`'deki örnek).
- `env` dosyası `chmod 600`, sahibi servis kullanıcısı.
- Bağımlılıklar için `cargo audit` CI'da çalıştırılır.
