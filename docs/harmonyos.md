<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: MPL-2.0
-->

# HarmonyOS map provider research

Day's canvas renderer remains the implemented map provider on HarmonyOS and OpenHarmony. A future native
provider can use the shared map state without changing the base crate or the Apple/Google crates.

Huawei's ArkTS Map Kit exposes `MapComponent`, `map.MapComponentController`, and `mapCommon`
through `@kit.MapKit`. The controller supports camera movement and animation, markers,
polylines, polygons, circles, and camera/tap events. Its documented capability is
`SystemCapability.Map.Core`, with a stage-model requirement. Marker creation is asynchronous.
These operations fit the shared `MapView` model, but the adapter must discard completions after
its node is removed. [Controller reference](https://developer.huawei.com/consumer/en/doc/doccenter-references/api/map-map-mapcomponentcontroller/).

Huawei Map Kit is a service dependency, not a capability to assume on every OpenHarmony image.
Its setup includes enabling the service in AppGallery Connect and configuring the application's
`client_id` and signing fingerprint. Authentication, quota, and network failures need separate
error handling. The native capability check, successful SDK initialization, and service
configuration must all succeed before reporting availability.
[Map Kit errors](https://developer.huawei.com/consumer/en/doc/harmonyos-references/errorcode-map/).

A provider should be packaged separately, for example `day-piece-map-huawei`, with an `arkui`
feature and `[package.metadata.day.harmony]` ArkTS contributions. Day already stages `ets`
directories with an `Index.ets` exporting a `DayPieceModule`; the map would register its own
piece kind there. Use a distinct service identity so another OpenHarmony map SDK can coexist.
The Rust base has no registry or closed provider enum to extend.

Before implementing the adapter:

- Verify the SDK's coordinate reference system for each supported region and convert at the
  provider boundary if needed. The common API promises WGS84. Do not reuse documentation for
  Huawei's older Android/HMS map SDK as evidence for the ArkTS SDK's coordinate behavior.
- Convert SDK zoom and logical/physical point units to the shared 512-point zoom convention.
  Validate camera round trips, antimeridian bounds, and high-latitude geometry on a real map.
- Map shared stable marker IDs to the asynchronous native handles. Remove event subscriptions
  and destroy handles when switching providers. A delayed `addMarker` result must not recreate
  an annotation removed while its request was pending.
- Keep Huawei search, routing, custom bubbles, tile overlays, and location-layer options in the
  provider API. They are not prerequisites for a portable basemap and marker view.
- Check API versions for optional features and `canIUse('SystemCapability.Map.Core')` for the
  device capability. API-level support alone does not grant service access.
- Keep location permissions separate from displaying a map. Huawei's my-location feature
  requires the relevant location permissions; the base can already show a position supplied
  by the application as an ordinary marker.
  [Huawei integration example](https://developer.huawei.com/consumer/en/doc/best-practices-V5/bpta-shared-bicycle-V5/).

The SDK coordinate-system details and a portable map SDK for arbitrary OpenHarmony images remain
implementation prerequisites. No native Huawei support is advertised by the current crates.
