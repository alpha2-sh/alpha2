import QuartzCore

extension Player {
    /// Draws the preview into `layer`, of `width` x `height` pixels, replacing any attached one.
    /// The player keeps its own reference to the layer until it is detached or replaced.
    public func attach(layer: CAMetalLayer, width: UInt32, height: UInt32) throws {
        let handle = UInt(bitPattern: Unmanaged.passUnretained(layer).toOpaque())
        try attachWindow(handle: UInt64(handle), width: width, height: height)
    }
}
