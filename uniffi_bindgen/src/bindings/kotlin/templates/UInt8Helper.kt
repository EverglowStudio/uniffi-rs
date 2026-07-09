/**
 * @suppress
 */
public object FfiConverterUByte: FfiConverter<UByte, Byte> {
    override fun lift(value: Byte): UByte {
        return value.toUByte()
    }

    fun lift(value: Int): UByte {
        return value.toUByte()
    }

    override fun read(buf: ByteBuffer): UByte {
        return lift(buf.get())
    }

    override fun lower(value: UByte): Byte {
        return value.toByte()
    }

    fun lowerForDirectCall(value: UByte): Int {
        return value.toInt()
    }

    override fun allocationSize(value: UByte) = 1UL

    override fun write(value: UByte, buf: ByteBuffer) {
        buf.put(value.toByte())
    }
}
