// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0
import Foundation
import MapKit
#if os(iOS)
import UIKit
private typealias MapColor = UIColor
#else
import AppKit
private typealias MapColor = NSColor
#endif

private typealias MapReport = @convention(c) (UInt64, UnsafePointer<CChar>) -> Void
private func coordinate(_ raw: Any?) -> CLLocationCoordinate2D {
    let p = raw as? [Double] ?? [0, 0]
    return CLLocationCoordinate2D(latitude: p.count > 1 ? p[1] : 0, longitude: p.first ?? 0)
}
private func mapColor(_ raw: Any?) -> MapColor {
    let c = raw as? [Double] ?? [0, 0, 0, 1]
    guard c.count == 4 else { return .clear }
    return MapColor(red: c[0], green: c[1], blue: c[2], alpha: c[3])
}
private final class DayAnnotation: MKPointAnnotation {
    let identity: String
    var tint: MapColor = .red
    init(_ identity: String) { self.identity = identity; super.init() }
}

private final class DayAppleMap: MKMapView, MKMapViewDelegate {
    var node: UInt64 = 0
    var report: MapReport?
    var disposed = false
    private var applying = false
    private var initialized = false
    private var initializationScheduled = false
    private var annotationsById: [String: DayAnnotation] = [:]
    private var overlayStyles: [ObjectIdentifier: [String: Any]] = [:]
    private var previousGeometry: NSArray = []
    private var serial = ""
    private var pending: [String: Any]?
    private var requestedPitch = 0.0

    func send(_ event: [String: Any]) {
        guard !disposed, let callback = report,
              let data = try? JSONSerialization.data(withJSONObject: event),
              let json = String(data: data, encoding: .utf8) else { return }
        json.withCString { callback(node, $0) }
    }
    func start() {
        delegate = self
        #if os(iOS)
        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped(_:)))
        tap.delegate = self
        tap.cancelsTouchesInView = false
        let doubleTap = UITapGestureRecognizer()
        doubleTap.delegate = self
        doubleTap.numberOfTapsRequired = 2
        doubleTap.cancelsTouchesInView = false
        addGestureRecognizer(doubleTap)
        tap.require(toFail: doubleTap)
        addGestureRecognizer(tap)
        #else
        let tap = NSClickGestureRecognizer(target: self, action: #selector(tapped(_:)))
        tap.delegate = self
        tap.numberOfClicksRequired = 1
        addGestureRecognizer(tap)
        #endif
    }
    #if os(iOS)
    @objc private func tapped(_ recognizer: UITapGestureRecognizer) { reportTap(recognizer.location(in: self)) }
    override func layoutSubviews() { super.layoutSubviews(); flushCamera() }
    override func didMoveToWindow() { super.didMoveToWindow(); flushCamera() }
    #else
    @objc private func tapped(_ recognizer: NSClickGestureRecognizer) { reportTap(recognizer.location(in: self)) }
    override func layout() { super.layout(); flushCamera() }
    override func setFrameSize(_ size: NSSize) { super.setFrameSize(size); flushCamera() }
    override func viewDidMoveToWindow() { super.viewDidMoveToWindow(); flushCamera() }
    #endif

    private func logicalPoint(_ point: CGPoint) -> CGPoint {
        #if os(macOS)
        return CGPoint(x: point.x, y: isFlipped ? point.y : bounds.height - point.y)
        #else
        return point
        #endif
    }
    private func reportTap(_ point: CGPoint) {
        if let annotation = annotationsById.values.first(where: { annotation in
            guard let view = view(for: annotation) else { return false }
            return view.bounds.contains(convert(point, to: view))
        }) {
            reportMarker(annotation, point)
            return
        }
        let location = convert(point, toCoordinateFrom: self)
        let logical = logicalPoint(point)
        send(["event":"tap", "lng":location.longitude, "lat":location.latitude,
              "x":logical.x, "y":logical.y])
    }
    func apply(_ data: [String: Any]) {
        guard !disposed else { return }
        if let options = data["options"] as? [String: Any] {
            mapType = ["Satellite": MKMapType.satellite, "Hybrid": .hybrid][options["mapType"] as? String ?? ""] ?? .standard
            showsTraffic = options["traffic"] as? Bool ?? false
            showsBuildings = options["buildings"] as? Bool ?? true
            showsCompass = options["compass"] as? Bool ?? true
            pointOfInterestFilter = (options["pointsOfInterest"] as? Bool ?? true) ? .includingAll : .excludingAll
            let pitch = options["pitch"] as? Double ?? 0
            if pitch != requestedPitch && initialized && pending == nil { pending = ["pitch": pitch] }
            requestedPitch = pitch
            isPitchEnabled = false
        }
        if let interaction = data["interaction"] as? [String: Any] {
            isScrollEnabled = interaction["pan"] as? Bool ?? true
            isZoomEnabled = interaction["zoomGesture"] as? Bool ?? true
            isRotateEnabled = interaction["rotate"] as? Bool ?? true
        }
        if let entries = data["markers"] as? [[String: Any]] { updateAnnotations(entries) }
        if let entries = data["overlays"] as? [[String: Any]] { updateOverlays(entries) }
        // Only app-originated camera edits arrive here. Annotation/options patches cannot
        // reapply an older camera, even when queued until layout or SDK initialization.
        if let camera = data["camera"] as? [String: Any] { pending = camera }
        if let request = data["request"] as? [String: Any] { pending = ["request": request] }
        flushCamera()
    }
    private func currentZoom() -> Double {
        // Measure map units per screen point, independently of heading and screen scale.
        let middle = CGPoint(x: bounds.midX, y: bounds.midY)
        let a = MKMapPoint(convert(middle, toCoordinateFrom: self))
        let b = MKMapPoint(convert(CGPoint(x: middle.x + 1, y: middle.y), toCoordinateFrom: self))
        var dx = abs(a.x - b.x)
        dx = min(dx, MKMapSize.world.width - dx)
        let units = hypot(dx, a.y - b.y)
        return max(0, min(22, log2(MKMapSize.world.width / (512 * max(units, 0.000001)))))
    }
    private func flushCamera(deferred: Bool = false) {
        guard !disposed, window != nil, bounds.width > 0, bounds.height > 0, let data = pending, !applying else { return }
        // MapKit installs its default region while attaching and laying out. Apply the app's
        // initial camera after that transaction, and ignore its initial delegate callbacks.
        if !initialized && !deferred {
            if !initializationScheduled {
                initializationScheduled = true
                DispatchQueue.main.async { [weak self] in
                    self?.initializationScheduled = false
                    self?.flushCamera(deferred: true)
                }
            }
            return
        }
        pending = nil
        applying = true
        var reportImmediate = false
        defer {
            applying = false
            initialized = true
            if reportImmediate { publishCamera() }
        }
        if let command = data["request"] as? [String: Any], let next = command["serial"] as? String, next != serial {
            serial = next
            let animated = (command["duration"] as? Double ?? 0) > 0
            reportImmediate = !animated
            if command["kind"] as? String == "bounds" {
                let sw = MKMapPoint(coordinate(command["sw"]))
                let ne = MKMapPoint(coordinate(command["ne"]))
                let width = ne.x >= sw.x ? ne.x - sw.x : ne.x + MKMapSize.world.width - sw.x
                let rect = MKMapRect(x: sw.x, y: min(sw.y, ne.y), width: max(width, 1), height: max(abs(sw.y - ne.y), 1))
                let padding = max(0, min(command["padding"] as? Double ?? 0, min(bounds.width, bounds.height) / 2 - 1))
                let old = camera.copy() as! MKMapCamera
                let north = old.copy() as! MKMapCamera
                north.heading = 0
                north.pitch = 0
                setCamera(north, animated: false)
                #if os(iOS)
                setVisibleMapRect(rect, edgePadding: UIEdgeInsets(top: padding, left: padding, bottom: padding, right: padding), animated: false)
                #else
                setVisibleMapRect(rect, edgePadding: NSEdgeInsets(top: padding, left: padding, bottom: padding, right: padding), animated: false)
                #endif
                let target = camera.copy() as! MKMapCamera
                target.heading = 0
                target.pitch = 0
                if animated { setCamera(old, animated: false) }
                setCamera(target, animated: animated)
            } else {
                move(center: coordinate(command["center"]), zoom: command["zoom"] as? Double ?? 0,
                     heading: command["bearing"] as? Double ?? camera.heading, animated: animated)
            }
        } else if data["center"] != nil {
            move(center: coordinate(data["center"]), zoom: data["zoom"] as? Double ?? 0,
                 heading: data["bearing"] as? Double ?? 0, animated: false)
            reportImmediate = true
        } else if data["pitch"] != nil {
            let target = camera.copy() as! MKMapCamera
            target.pitch = requestedPitch
            setCamera(target, animated: false)
            reportImmediate = true
        }
    }
    private func move(center: CLLocationCoordinate2D, zoom: Double, heading: Double, animated: Bool) {
        // MapKit uses camera distance; derive it from a north-up region at the common zoom.
        let units = MKMapSize.world.width / (512 * pow(2, zoom))
        let point = MKMapPoint(center)
        let rect = MKMapRect(x:point.x - bounds.width * units / 2,
                             y:point.y - bounds.height * units / 2,
                             width:bounds.width * units, height:bounds.height * units)
        // Compute the camera via a temporary region change, then animate from the old camera.
        let old = camera.copy() as! MKMapCamera
        setVisibleMapRect(rect, animated: false)
        let target = camera.copy() as! MKMapCamera
        target.centerCoordinate = center
        target.heading = heading
        target.pitch = requestedPitch
        if animated { setCamera(old, animated: false) }
        setCamera(target, animated: animated)
    }
    private func updateAnnotations(_ entries: [[String: Any]]) {
        let ids = Set(entries.compactMap { $0["id"] as? String })
        for (id, annotation) in annotationsById where !ids.contains(id) {
            removeAnnotation(annotation)
            annotationsById.removeValue(forKey: id)
        }
        for entry in entries {
            guard let id = entry["id"] as? String else { continue }
            let existing = annotationsById[id]
            let annotation = existing ?? DayAnnotation(id)
            annotation.coordinate = coordinate(entry["position"])
            annotation.title = entry["title"] as? String
            annotation.subtitle = entry["subtitle"] as? String
            annotation.tint = mapColor(entry["color"])
            annotationsById[id] = annotation
            if existing == nil { addAnnotation(annotation) }
            (view(for: annotation) as? MKMarkerAnnotationView)?.markerTintColor = annotation.tint
        }
    }
    private func updateOverlays(_ entries: [[String: Any]]) {
        let array = entries as NSArray
        guard !array.isEqual(to: previousGeometry as! [Any]) else { return }
        previousGeometry = array
        removeOverlays(overlays)
        overlayStyles.removeAll()
        for entry in entries {
            let points = (entry["points"] as? [[Double]] ?? []).map { coordinate($0) }
            let shape: MKOverlay
            switch entry["kind"] as? String {
            case "line":
                guard points.count >= 2 else { continue }
                shape = MKPolyline(coordinates: points, count: points.count)
            case "polygon":
                guard points.count >= 3 else { continue }
                shape = MKPolygon(coordinates: points, count: points.count)
            case "circle":
                shape = MKCircle(center: coordinate(entry["center"]), radius: entry["radius"] as? Double ?? 0)
            default: continue
            }
            overlayStyles[ObjectIdentifier(shape)] = entry
            addOverlay(shape)
        }
    }
    func mapView(_ mapView: MKMapView, rendererFor overlay: MKOverlay) -> MKOverlayRenderer {
        let renderer: MKOverlayPathRenderer
        if let line = overlay as? MKPolyline { renderer = MKPolylineRenderer(polyline: line) }
        else if let polygon = overlay as? MKPolygon { renderer = MKPolygonRenderer(polygon: polygon) }
        else if let circle = overlay as? MKCircle { renderer = MKCircleRenderer(circle: circle) }
        else { return MKOverlayRenderer(overlay: overlay) }
        let style = overlayStyles[ObjectIdentifier(overlay)] ?? [:]
        renderer.strokeColor = mapColor(style["stroke"])
        renderer.fillColor = mapColor(style["fill"])
        renderer.lineWidth = style["width"] as? Double ?? 1
        return renderer
    }
    func mapView(_ mapView: MKMapView, viewFor annotation: MKAnnotation) -> MKAnnotationView? {
        guard let annotation = annotation as? DayAnnotation else { return nil }
        let view = (dequeueReusableAnnotationView(withIdentifier: "day-marker") as? MKMarkerAnnotationView)
            ?? MKMarkerAnnotationView(annotation: annotation, reuseIdentifier: "day-marker")
        view.annotation = annotation
        view.markerTintColor = annotation.tint
        view.canShowCallout = !(annotation.title ?? "").isEmpty
        return view
    }
    private func reportMarker(_ annotation: DayAnnotation, _ point: CGPoint) {
        let p = logicalPoint(point)
        send(["event":"tap", "marker":annotation.identity, "lng":annotation.coordinate.longitude,
              "lat":annotation.coordinate.latitude, "x":p.x, "y":p.y])
    }
    func mapView(_ mapView: MKMapView, regionDidChangeAnimated animated: Bool) { publishCamera() }
    private func publishCamera() {
        guard initialized, !applying, bounds.width > 0, bounds.height > 0 else { return }
        send(["event":"camera", "lng":centerCoordinate.longitude, "lat":centerCoordinate.latitude,
              "zoom":currentZoom(), "bearing":camera.heading])
    }
    func mapViewDidFinishRenderingMap(_ mapView: MKMapView, fullyRendered: Bool) {
        if fullyRendered { send(["event":"ready"]) }
    }
    func mapViewDidFailLoadingMap(_ mapView: MKMapView, withError error: Error) {
        send(["event":"error", "code":"map-load-failed"])
    }
    func dispose() {
        disposed = true
        report = nil
        delegate = nil
        pending = nil
        removeAnnotations(annotations)
        removeOverlays(overlays)
        annotationsById.removeAll()
        overlayStyles.removeAll()
    }
}
// Reporting taps must not consume MapKit's native pan, pinch, selection or double-tap gestures.
#if os(iOS)
extension DayAppleMap: UIGestureRecognizerDelegate {
    func gestureRecognizer(_ recognizer: UIGestureRecognizer,
        shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool { true }
}
#else
extension DayAppleMap: NSGestureRecognizerDelegate {
    func gestureRecognizer(_ recognizer: NSGestureRecognizer,
        shouldRecognizeSimultaneouslyWith other: NSGestureRecognizer) -> Bool { true }
}
#endif

private func decode(_ text: UnsafePointer<CChar>) -> [String: Any] {
    let data = Data(String(cString: text).utf8)
    return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
}
@_cdecl("day_apple_map_new")
public func dayAppleMapNew(_ node: UInt64, _ text: UnsafePointer<CChar>,
    _ callback: @escaping @convention(c) (UInt64, UnsafePointer<CChar>) -> Void) -> UnsafeMutableRawPointer {
    let view = DayAppleMap(frame: .zero)
    view.node = node
    view.report = callback
    view.start()
    view.apply(decode(text))
    return Unmanaged.passRetained(view).toOpaque()
}
@_cdecl("day_apple_map_update")
public func dayAppleMapUpdate(_ ptr: UnsafeMutableRawPointer, _ text: UnsafePointer<CChar>) {
    Unmanaged<DayAppleMap>.fromOpaque(ptr).takeUnretainedValue().apply(decode(text))
}
@_cdecl("day_apple_map_release")
public func dayAppleMapRelease(_ ptr: UnsafeMutableRawPointer) {
    Unmanaged<DayAppleMap>.fromOpaque(ptr).takeUnretainedValue().dispose()
}
