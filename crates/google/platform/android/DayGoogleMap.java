// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
package dev.daybrite.day.piece.maps;

import android.app.Activity;
import android.app.Application;
import android.content.ComponentCallbacks2;
import android.content.res.Configuration;
import android.content.pm.PackageManager;
import android.os.Bundle;
import android.view.View;
import android.widget.FrameLayout;
import com.google.android.gms.common.GoogleApiAvailability;
import com.google.android.gms.common.ConnectionResult;
import com.google.android.gms.maps.CameraUpdate;
import com.google.android.gms.maps.CameraUpdateFactory;
import com.google.android.gms.maps.GoogleMap;
import com.google.android.gms.maps.MapView;
import com.google.android.gms.maps.model.*;
import dev.daybrite.day.bridge.DayBridge;
import org.json.JSONArray;
import org.json.JSONObject;
import org.json.JSONException;
import java.util.HashMap;
import java.util.HashSet;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Set;

/** A MapView with explicit Activity lifecycle forwarding and release-time cleanup. */
public final class DayGoogleMap extends FrameLayout
        implements Application.ActivityLifecycleCallbacks, ComponentCallbacks2 {
    private final long node;
    private final Activity activity;
    private MapView nativeView;
    private GoogleMap map;
    private JSONObject pending;
    private final Map<String, Marker> markers = new HashMap<>();
    private final List<Polyline> lines = new ArrayList<>();
    private final List<Polygon> polygons = new ArrayList<>();
    private final List<Circle> circles = new ArrayList<>();
    private String geometry = "";
    private String style = "";
    private String serial = "";
    private float pitch;
    private boolean disposed;
    private boolean registered;
    private boolean started;
    private boolean resumed;
    private boolean foreground = true;
    private boolean applying;
    private boolean firstCamera = true;

    private DayGoogleMap(long node, String json) {
        super(DayBridge.ctx);
        this.node = node;
        this.activity = DayBridge.ctx instanceof Activity ? (Activity) DayBridge.ctx : null;
        try {
            pending = new JSONObject(json);
            if (activity == null) { error("missing-activity"); return; }
            Bundle metadata = activity.getPackageManager().getApplicationInfo(
                    activity.getPackageName(), PackageManager.GET_META_DATA).metaData;
            String key = metadata == null ? "" : metadata.getString("com.google.android.geo.API_KEY", "");
            if (key.trim().isEmpty()) { error("missing-api-key"); return; }
            if (GoogleApiAvailability.getInstance().isGooglePlayServicesAvailable(activity)
                    != ConnectionResult.SUCCESS) { error("play-services-unavailable"); return; }
            nativeView = new MapView(activity);
            nativeView.onCreate(null);
            addView(nativeView, new FrameLayout.LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT));
            activity.getApplication().registerActivityLifecycleCallbacks(this);
            activity.registerComponentCallbacks(this);
            registered = true;
            nativeView.getMapAsync(ready -> {
                if (disposed) return;
                map = ready;
                map.setOnCameraIdleListener(() -> cameraChanged());
                map.setOnMapClickListener(location -> tap(location, null));
                map.setOnMarkerClickListener(marker -> { tap(marker.getPosition(), (String) marker.getTag()); return false; });
                map.setOnMapLoadedCallback(() -> send("ready", new JSONObject()));
                apply();
            });
        } catch (Exception exception) { error("map-initialization-failed"); }
    }
    public static View create(long node, String json) { return new DayGoogleMap(node, json); }
    public static void update(View view, String json) {
        if (!(view instanceof DayGoogleMap)) return;
        DayGoogleMap host = (DayGoogleMap) view;
        if (host.disposed) return;
        try {
            JSONObject patch = new JSONObject(json);
            java.util.Iterator<String> keys = patch.keys();
            while (keys.hasNext()) { String key = keys.next(); host.pending.put(key, patch.get(key)); }
            // An explicit camera write supersedes a queued animation and vice versa.
            if (patch.has("camera")) host.pending.remove("request");
            if (patch.has("request")) host.pending.remove("camera");
            host.apply();
        }
        catch (JSONException exception) { host.error("invalid-map-state"); }
    }
    public static void dispose(View view) { if (view instanceof DayGoogleMap) ((DayGoogleMap) view).close(); }
    private void send(String event, JSONObject value) {
        if (disposed) return;
        try { value.put("event", event); DayBridge.nativeOnEvent(node, 12, 0, value.toString()); }
        catch (JSONException exception) { /* The event contains only finite SDK coordinates. */ }
    }
    private void error(String code) {
        JSONObject event = new JSONObject();
        try { event.put("code", code); send("error", event); }
        catch (JSONException exception) { /* A constant code is always JSON-compatible. */ }
    }
    private static LatLng position(JSONArray point) throws JSONException {
        return new LatLng(point.getDouble(1), point.getDouble(0));
    }
    private static int color(JSONArray value) throws JSONException {
        return android.graphics.Color.argb(channel(value.getDouble(3)), channel(value.getDouble(0)),
                channel(value.getDouble(1)), channel(value.getDouble(2)));
    }
    private static int channel(double value) { return (int) Math.round(Math.max(0, Math.min(1, value)) * 255); }
    private float density() { return getResources().getDisplayMetrics().density; }
    private void apply() {
        if (disposed || map == null || pending == null || applying) return;
        applying = true;
        try {
            JSONObject o = pending.optJSONObject("options");
            if (o != null) {
                String type = o.optString("mapType", "Normal");
                map.setMapType(type.equals("Satellite") ? GoogleMap.MAP_TYPE_SATELLITE :
                        type.equals("Terrain") ? GoogleMap.MAP_TYPE_TERRAIN : type.equals("Hybrid") ? GoogleMap.MAP_TYPE_HYBRID :
                        type.equals("None") ? GoogleMap.MAP_TYPE_NONE : GoogleMap.MAP_TYPE_NORMAL);
                map.setTrafficEnabled(o.optBoolean("traffic"));
                map.setBuildingsEnabled(o.optBoolean("buildings", true));
                map.setIndoorEnabled(o.optBoolean("indoor", true));
                map.getUiSettings().setCompassEnabled(o.optBoolean("compass", true));
                map.getUiSettings().setZoomControlsEnabled(o.optBoolean("zoomControls"));
                map.getUiSettings().setTiltGesturesEnabled(false);
                String nextStyle = o.isNull("style") ? "" : o.optString("style");
                if (!style.equals(nextStyle)) {
                    if (!map.setMapStyle(nextStyle.isEmpty() ? null : new MapStyleOptions(nextStyle))) error("invalid-map-style");
                    style = nextStyle;
                }
                float nextPitch = (float) o.optDouble("pitch");
                if (nextPitch != pitch && !firstCamera && !pending.has("camera") && !pending.has("request"))
                    map.moveCamera(CameraUpdateFactory.newCameraPosition(new CameraPosition.Builder(map.getCameraPosition()).tilt(nextPitch).build()));
                pitch = nextPitch;
                pending.remove("options");
            }
            JSONObject gestures = pending.optJSONObject("interaction");
            if (gestures != null) {
                map.getUiSettings().setScrollGesturesEnabled(gestures.optBoolean("pan", true));
                map.getUiSettings().setZoomGesturesEnabled(gestures.optBoolean("zoomGesture", true));
                map.getUiSettings().setRotateGesturesEnabled(gestures.optBoolean("rotate", true));
                pending.remove("interaction");
            }
            if (pending.has("markers")) { updateMarkers(pending.getJSONArray("markers")); pending.remove("markers"); }
            if (pending.has("overlays")) { updateOverlays(pending.getJSONArray("overlays")); pending.remove("overlays"); }
            if (getWidth() <= 0 || getHeight() <= 0) return;
            JSONObject command = pending.optJSONObject("request");
            JSONObject camera = pending.optJSONObject("camera");
            pending.remove("request");
            pending.remove("camera");
            if (command != null && !serial.equals(command.getString("serial"))) {
                serial = command.getString("serial");
                CameraUpdate update;
                if (command.getString("kind").equals("bounds")) {
                    LatLng sw = position(command.getJSONArray("sw"));
                    LatLng ne = position(command.getJSONArray("ne"));
                    LatLngBounds bounds = new LatLngBounds(sw, ne);
                    int padding = (int) Math.min(command.optDouble("padding") * density(), Math.min(getWidth(), getHeight()) / 2.0 - 1);
                    update = CameraUpdateFactory.newLatLngBounds(bounds, Math.max(0, padding));
                } else {
                    update = CameraUpdateFactory.newCameraPosition(new CameraPosition.Builder()
                        .target(position(command.getJSONArray("center")))
                        .zoom((float) command.getDouble("zoom") + 1)
                        .bearing((float) command.optDouble("bearing", map.getCameraPosition().bearing))
                        .tilt(pitch).build());
                }
                double duration = command.optDouble("duration");
                firstCamera = false;
                if (duration > 0) map.animateCamera(update, Math.max(1, (int) (duration * 1000)), null);
                else map.moveCamera(update);
            } else if (camera != null) {
                LatLng center = position(camera.getJSONArray("center"));
                float zoom = (float) camera.getDouble("zoom") + 1; // Google uses a 256-dp world.
                float bearing = (float) camera.optDouble("bearing");
                firstCamera = false;
                map.moveCamera(CameraUpdateFactory.newCameraPosition(new CameraPosition(center, zoom, pitch, bearing)));
            }
        } catch (Exception exception) { error("map-update-failed"); }
        finally { applying = false; }
    }
    private void updateMarkers(JSONArray entries) throws JSONException {
        Set<String> live = new HashSet<>();
        for (int i=0; i<entries.length(); i++) {
            JSONObject entry = entries.getJSONObject(i);
            String id = entry.getString("id");
            live.add(id);
            Marker marker = markers.get(id);
            LatLng location = position(entry.getJSONArray("position"));
            int tint = color(entry.getJSONArray("color"));
            float[] hsv = new float[3]; android.graphics.Color.colorToHSV(tint, hsv);
            if (marker == null) {
                marker = map.addMarker(new MarkerOptions().position(location));
                if (marker == null) continue;
                marker.setTag(id); markers.put(id, marker);
            }
            marker.setPosition(location);
            marker.setTitle(entry.optString("title"));
            marker.setSnippet(entry.optString("subtitle"));
            marker.setIcon(BitmapDescriptorFactory.defaultMarker(hsv[0]));
            marker.setAlpha(android.graphics.Color.alpha(tint) / 255f);
        }
        for (String id : new HashSet<>(markers.keySet())) if (!live.contains(id)) markers.remove(id).remove();
    }
    private void updateOverlays(JSONArray entries) throws JSONException {
        String encoded = entries.toString();
        if (geometry.equals(encoded)) return;
        for (Polyline line : lines) line.remove(); lines.clear();
        for (Polygon polygon : polygons) polygon.remove(); polygons.clear();
        for (Circle circle : circles) circle.remove(); circles.clear();
        geometry = encoded;
        for (int i=0; i<entries.length(); i++) {
            JSONObject entry = entries.getJSONObject(i);
            float width = (float) entry.optDouble("width", 1) * density();
            int stroke = color(entry.getJSONArray("stroke"));
            String kind = entry.getString("kind");
            List<LatLng> points = new ArrayList<>();
            JSONArray path = entry.optJSONArray("points");
            if (path != null) for (int j=0; j<path.length(); j++) points.add(position(path.getJSONArray(j)));
            if (kind.equals("line") && points.size() >= 2)
                lines.add(map.addPolyline(new PolylineOptions().addAll(points).color(stroke).width(width).zIndex(i)));
            else if (kind.equals("polygon") && points.size() >= 3)
                polygons.add(map.addPolygon(new PolygonOptions().addAll(points).strokeColor(stroke).strokeWidth(width)
                        .fillColor(color(entry.getJSONArray("fill"))).zIndex(i)));
            else if (kind.equals("circle"))
                circles.add(map.addCircle(new CircleOptions().center(position(entry.getJSONArray("center")))
                        .radius(entry.optDouble("radius")).strokeColor(stroke).strokeWidth(width)
                        .fillColor(color(entry.getJSONArray("fill"))).zIndex(i)));
        }
    }
    private void cameraChanged() {
        if (disposed || map == null || applying || firstCamera) return;
        CameraPosition camera = map.getCameraPosition();
        try { JSONObject event = new JSONObject();
            event.put("lng",camera.target.longitude).put("lat",camera.target.latitude)
                .put("zoom",camera.zoom - 1).put("bearing",camera.bearing);
            send("camera",event);
        } catch (JSONException exception) { error("invalid-camera"); }
    }
    private void tap(LatLng location, String marker) {
        if (disposed || map == null) return;
        android.graphics.Point point = map.getProjection().toScreenLocation(location);
        try { JSONObject event = new JSONObject();
            event.put("lng",location.longitude).put("lat",location.latitude).put("marker",marker)
                .put("x",point.x / density()).put("y",point.y / density());
            send("tap",event);
        } catch (JSONException exception) { error("invalid-tap"); }
    }
    @Override protected void onSizeChanged(int w, int h, int oldw, int oldh) { super.onSizeChanged(w,h,oldw,oldh); apply(); }
    @Override protected void onAttachedToWindow() { super.onAttachedToWindow(); syncLifecycle(); }
    @Override protected void onDetachedFromWindow() { pause(); super.onDetachedFromWindow(); }
    private void syncLifecycle() {
        if (disposed || nativeView == null || !isAttachedToWindow() || !foreground) return;
        if (!started) { nativeView.onStart(); started = true; }
        if (!resumed) { nativeView.onResume(); resumed = true; }
    }
    private void pause() {
        if (nativeView == null) return;
        if (resumed) { nativeView.onPause(); resumed = false; }
        if (started) { nativeView.onStop(); started = false; }
    }
    private void close() {
        if (disposed) return;
        disposed = true;
        if (registered) {
            activity.getApplication().unregisterActivityLifecycleCallbacks(this);
            activity.unregisterComponentCallbacks(this); registered = false;
        }
        if (map != null) {
            map.setOnCameraIdleListener(null); map.setOnMapClickListener(null);
            map.setOnMarkerClickListener(null); map.setOnMapLoadedCallback(null);
        }
        pause();
        if (nativeView != null) nativeView.onDestroy();
        map = null; nativeView = null; pending = null;
        markers.clear(); lines.clear(); polygons.clear(); circles.clear();
        removeAllViews();
    }
    @Override public void onActivityResumed(Activity a) { if (a == activity) { foreground=true; syncLifecycle(); } }
    @Override public void onActivityPaused(Activity a) { if (a == activity) { foreground=false; pause(); } }
    @Override public void onActivityDestroyed(Activity a) { if (a == activity) close(); }
    // Day recreates leaves from MapView state. Node IDs are not persistence keys, and this
    // host has no saved-instance-state restoration hook. Do not write an unreadable Bundle.
    @Override public void onActivitySaveInstanceState(Activity a, Bundle state) {}
    @Override public void onLowMemory() { if (nativeView != null && !disposed) nativeView.onLowMemory(); }
    @Override public void onTrimMemory(int level) { if (level >= ComponentCallbacks2.TRIM_MEMORY_RUNNING_LOW) onLowMemory(); }
    @Override public void onConfigurationChanged(Configuration configuration) {}
    @Override public void onActivityCreated(Activity a, Bundle state) {}
    @Override public void onActivityStarted(Activity a) {}
    @Override public void onActivityStopped(Activity a) { if (a == activity) pause(); }
}
