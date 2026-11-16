# Map Demo UI strings (https://daybrite.dev/docs/how-to/localization). Add a locale by dropping a
# sibling folder (locales/fr/app.ftl) and translating.

app_title = Map Demo

# The style picker's entries: the hosted styles and the built-in sample.
source_liberty = OpenFreeMap Liberty
source_bright = OpenFreeMap Bright
source_positron = OpenFreeMap Positron
source_fiord = OpenFreeMap Fiord
source_dark = OpenFreeMap Dark
source_colorful = VersaTiles Colorful
source_colorful_dark = VersaTiles Colorful Dark
source_sample = Built-in sample
source_liberty_offline = Liberty, recorded (offline)

# The language picker's first entry: the style's own labels.
language_default = Map language

zoom_in = +
zoom_out = −

# The camera row: fly between the two markers, fit both, turn by 45°, back to north up.
fly = Fly
fit = Fit
rotate = Rotate
north = North
bearing_north = North up
bearing = Bearing { $degrees }°

# What the last tap found.
tap_prompt = Tap the map
tap_marker = Marker: { $title }
tap_feature = { $layer }: { $name } ({ $count } features)
tap_nothing = Nothing there

# The readout under the map.
status_loading = Loading
status_ready = Ready
status_failed = Failed: { $reason }
position = { $lat }, { $lng } at zoom { $zoom }
frame = { $tiles } tiles drawn, { $labels } labels, { $pending } loading

# Map provider selection and setup.
map_provider_osm = OpenStreetMap
map_provider_apple = Apple Maps
map_provider_google = Google Maps
map_missing_api_key = Configure a Google Maps API key for this Android app.
map_play_services_unavailable = Google Maps requires Google Play services.
map_load_failed = The map could not load. Check the connection and provider configuration.

language_name =
    { $code ->
        [am] Amharic
        [ar] Arabic
        [az] Azerbaijani
        [be] Belarusian
        [bg] Bulgarian
        [br] Breton
        [bs] Bosnian
        [ca] Catalan
        [co] Corsican
        [cs] Czech
        [cy] Welsh
        [da] Danish
        [de] German
        [el] Greek
        [en] English
        [eo] Esperanto
        [es] Spanish
        [et] Estonian
        [eu] Basque
        [fi] Finnish
        [fr] French
        [fy] Western Frisian
        [ga] Irish
        [gd] Scottish Gaelic
        [he] Hebrew
        [hi] Hindi
        [hr] Croatian
        [hu] Hungarian
        [hy] Armenian
        [id] Indonesian
        [is] Icelandic
        [it] Italian
        [ja] Japanese
        [ka] Georgian
        [kk] Kazakh
        [kn] Kannada
        [ko] Korean
        [ku] Kurdish
        [la] Latin
        [lb] Luxembourgish
        [lt] Lithuanian
        [lv] Latvian
        [mk] Macedonian
        [ml] Malayalam
        [mt] Maltese
        [nl] Dutch
        [no] Norwegian
        [oc] Occitan
        [pl] Polish
        [pt] Portuguese
        [rm] Romansh
        [ro] Romanian
        [ru] Russian
        [sk] Slovak
        [sl] Slovenian
        [sq] Albanian
        [sr] Serbian
        [sv] Swedish
        [ta] Tamil
        [te] Telugu
        [th] Thai
        [tr] Turkish
        [uk] Ukrainian
        [vi] Vietnamese
        [zh] Chinese
       *[other] { $code }
    }
sight_island_center = Island center
sight_east_coast = East coast
sight_trafalgar_square = Trafalgar Square
sight_elizabeth_tower = Elizabeth Tower
