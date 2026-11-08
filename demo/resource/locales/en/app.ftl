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
