{%- let item_type_name = item_type|type_name(ci) %}
{%- let error_type_name = error_type|type_name(ci) %}

/**
 * @suppress
 */
@Suppress("UNUSED_PARAMETER")
public object {{ ffi_converter_name }}: FfiConverter<Flow<{{ item_type_name }}>, Long> {
    override fun lift(value: Long): Flow<{{ item_type_name }}> {
        throw InternalException("Input stream values are only supported as direct function arguments")
    }

    override fun lower(value: Flow<{{ item_type_name }}>): Long =
        uniffiCreateInputStream(
            value,
            { next -> lowerNext(next) },
            { error ->
                if (error is {{ error_type_name }}) {
                    {{ error_type|lower_fn }}(error)
                } else {
                    null
                }
            },
        )

    override fun read(buf: ByteBuffer): Flow<{{ item_type_name }}> {
        throw InternalException("Input stream values are only supported as direct function arguments")
    }

    override fun allocationSize(value: Flow<{{ item_type_name }}>): ULong {
        throw InternalException("Input stream values are only supported as direct function arguments")
    }

    override fun write(value: Flow<{{ item_type_name }}>, buf: ByteBuffer) {
        throw InternalException("Input stream values are only supported as direct function arguments")
    }

    private fun lowerNext(next: UniffiInputStreamNext<{{ item_type_name }}>): RustBuffer.ByValue =
        when (next) {
            is UniffiInputStreamNext.Item -> lowerNextItem(next.value)
            UniffiInputStreamNext.Done -> lowerNextDone()
        }

    private fun lowerNextDone(): RustBuffer.ByValue {
        val rbuf = RustBuffer.alloc(1UL)
        try {
            val bbuf = rbuf.data!!.getByteBuffer(0, rbuf.capacity).also {
                it.order(ByteOrder.BIG_ENDIAN)
            }
            bbuf.put(0)
            rbuf.writeField("len", bbuf.position().toLong())
            return rbuf
        } catch (e: Throwable) {
            RustBuffer.free(rbuf)
            throw e
        }
    }

    private fun lowerNextItem(value: {{ item_type_name }}): RustBuffer.ByValue {
        val rbuf = RustBuffer.alloc(1UL + {{ item_type|allocation_size_fn }}(value))
        try {
            val bbuf = rbuf.data!!.getByteBuffer(0, rbuf.capacity).also {
                it.order(ByteOrder.BIG_ENDIAN)
            }
            bbuf.put(1)
            {{ item_type|write_fn }}(value, bbuf)
            rbuf.writeField("len", bbuf.position().toLong())
            return rbuf
        } catch (e: Throwable) {
            RustBuffer.free(rbuf)
            throw e
        }
    }
}
