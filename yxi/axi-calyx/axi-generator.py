from calyx.builder import (
    Builder,
    add_comp_params,
    invoke,
    while_with,
    par,
    while_,
)
from typing import Literal
from math import log2, ceil
import json

# In general, ports to the wrapper are uppercase, internal registers are lower case.

yxi_input = """
{
  "toplevel": "main",
  "memories": [
    {
      "name": "A0",
      "width": 32,
      "size": 8
    },
    {
      "name": "B0",
      "width": 32,
      "size": 8
    },
    {
      "name": "Sum0",
      "width": 32,
      "size": 8
    }
  ]
}
"""

yxi = json.loads(yxi_input)
mems = yxi["memories"]


def add_arread_channel(prog, mem):
    _add_m_to_s_address_channel(prog, mem, "AR")


def add_awwrite_channel(prog, mem):
    aw_channel = _add_m_to_s_address_channel(prog, mem, "AW")
    max_transfers = aw_channel.reg("max_transfers", 8, is_ref=True)

    # TODO(nathanielnrn): We eventually want to move beyond
    # the implicit 1 transaction that is the size of the memory
    # How should we store this?
    # Recall this goes to write channel as number of transfers it expectes to do before
    # setting WLAST high
    with aw_channel.get_group("do_aw_transfer"):
        max_transfers.in_ = mem["size"] - 1
        max_transfers.write_en = 1


def _add_m_to_s_address_channel(prog, mem, prefix: Literal["AW", "AR"]):
    """Adds a manager to subordinate
    channel to the program. Uses `prefix` to name the channels
    appropriately. Expected to be either "AW" or "AR."
    Contains all of the channels shared between AW and AR channels.

    Returns a component builder in case there are additional
    cells/wires/groups that need to be added to the component.
    """

    assert prefix in ["AW", "AR"], "Prefix must be either AW or AR."

    # Following Arm's notation of denoting `xVALID` and `xREADY` signals
    # `x` stands for the prefix of the channel, i.e. `AW` or `AR`
    lc_x = prefix.lower()
    x = prefix
    # Inputs/outputs
    m_to_s_address_channel = prog.component(f"m_{lc_x}_channel")
    channel_inputs = [("ARESETn", 1), (f"{x}READY", 1)]
    channel_outputs = [
        (f"{x}VALID", 1),
        (f"{x}ADDR", 64),
        (f"{x}SIZE", 3),  # bytes used in transfer
        (f"{x}LEN", 8),  # number of transfers in transaction
        (f"{x}BURST", 2),  # for XRT should be tied to 2'b01 for WRAP burst
        (f"{x}PROT", 3),  # tied to be priviliged, nonsecure, data access request
    ]
    add_comp_params(m_to_s_address_channel, channel_inputs, channel_outputs)

    # Cells
    xvalid = m_to_s_address_channel.reg(f"{lc_x}valid", 1)
    xhandshake_occurred = m_to_s_address_channel.reg(f"{lc_x}_handshake_occurred", 1)
    curr_addr_axi = m_to_s_address_channel.reg("curr_addr_axi", 64, is_ref=True)
    xlen = m_to_s_address_channel.reg(f"{lc_x}len", 8)

    # Number of txns we want to occur before m_arread_channel is done
    # TODO: parameterize
    txn_n = m_to_s_address_channel.const("txn_n", 32, 1)
    txn_count = m_to_s_address_channel.reg("txn_count", 32)
    txn_adder = m_to_s_address_channel.add(32, "txn_adder")

    # Need to put block_transfer register here to avoid combinational loops
    bt_reg = m_to_s_address_channel.reg("bt_reg", 1)

    # Wires
    with m_to_s_address_channel.continuous:
        m_to_s_address_channel.this()[f"{x}VALID"] = xvalid.out

    # Groups
    # Responsible for asserting ARVALID, and deasserting it a cycle after the handshake.
    # This is necesarry because of the way transitions between groups work.
    # See #1828 https://github.com/calyxir/calyx/issues/1828
    with m_to_s_address_channel.group(f"do_{lc_x}_transfer") as do_x_transfer:
        xREADY = m_to_s_address_channel.this()[f"{x}READY"]
        xvalid.in_ = (~(xvalid.out & xREADY) & ~xhandshake_occurred.out) @ 1
        # Deassert in the next cycle once it is high
        xvalid.in_ = ((xvalid.out & xREADY) | xhandshake_occurred.out) @ 0
        xvalid.write_en = 1

        xhandshake_occurred.in_ = (xvalid.out & xREADY) @ 1
        xhandshake_occurred.write_en = (~xhandshake_occurred.out) @ 1

        # Drive output signals for transfer
        m_to_s_address_channel.this()[f"{x}ADDR"] = curr_addr_axi.out
        # This is taken from mem size, we assume the databus width is the size
        # of our memory cell and that width is a power of 2
        # TODO(nathanielnrn): convert to binary instead of decimal
        m_to_s_address_channel.this()[f"{x}SIZE"] = width_xsize(mem["width"])
        # TODO(nathanielnrn): Figure our how to set arlen. For now set to size of mem.
        m_to_s_address_channel.this()[f"{x}LEN"] = xlen.out
        m_to_s_address_channel.this()[f"{x}BURST"] = 1  # Must be INCR for XRT
        # Required by spec, we hardcode to privileged, non-secure, data access
        m_to_s_address_channel.this()[f"{x}PROT"] = 0b110

        # control block_transfer reg to go low after one cycle
        bt_reg.in_ = (xREADY & xvalid.out) @ 1
        bt_reg.in_ = ~(xREADY & xvalid.out) @ 0
        bt_reg.write_en = 1
        do_x_transfer.done = bt_reg.out

    with m_to_s_address_channel.group("incr_txn_count") as incr_txn_count:
        txn_adder.left = txn_count.out
        txn_adder.right = 1
        txn_count.in_ = txn_adder.out
        txn_count.write_en = 1
        incr_txn_count.done = txn_count.done

    # Control
    # check if txn_count == txn_n
    cellname = "perform_reads" if prefix == "AR" else "perform_writes"
    check_transactions_done = m_to_s_address_channel.neq_use(
        txn_count.out, txn_n.out, signed=False, cellname=cellname, width=32
    )

    invoke_txn_count = invoke(txn_count, in_in=0)
    # ARLEN must be between 0-255, make sure to subtract 1 from yxi
    # size when assigning to ARLEN
    assert mem["size"] < 256, "Memory size must be less than 256"
    invoke_xlen = invoke(xlen, in_in=mem["size"] - 1)

    while_body = [
        par(
            invoke(bt_reg, in_in=0),
            invoke(xhandshake_occurred, in_in=0),
        ),
        do_x_transfer,
        invoke(xvalid, in_in=0),
        incr_txn_count,
    ]

    while_loop = while_with(check_transactions_done, while_body)
    m_to_s_address_channel.control += [invoke_txn_count, invoke_xlen, while_loop]
    return m_to_s_address_channel


def add_read_channel(prog, mem):
    # Inputs/Outputs
    read_channel = prog.component("m_read_channel")
    # TODO(nathanielnrn): We currently assume RDATA is the same width as the
    # memory. This limits throughput many AXI data busses are much wider
    # i.e., 512 bits.
    channel_inputs = [
        ("ARESETn", 1),
        ("RVALID", 1),
        ("RLAST", 1),
        ("RDATA", mem["width"]),
        ("RRESP", 2),
    ]
    channel_outputs = [("RREADY", 1)]
    add_comp_params(read_channel, channel_inputs, channel_outputs)

    # Cells

    # We assume idx_size is exactly clog2(len). See comment in #1751
    # https://github.com/calyxir/calyx/issues/1751#issuecomment-1778360566
    mem_ref = read_channel.seq_mem_d1(
        name="mem_ref",
        bitwidth=mem["width"],
        len=mem["size"],
        idx_size=clog2(mem["size"]),
        is_external=False,
        is_ref=True,
    )

    # according to zipcpu, rready should be registered
    rready = read_channel.reg("rready", 1)
    curr_addr_internal_mem = read_channel.reg(
        "curr_addr_internal_mem", clog2(mem["size"]), is_ref=True
    )
    curr_addr_axi = read_channel.reg("curr_addr_axi", 64, is_ref=True)
    # Registed because RLAST is high with laster transfer, not after
    # before this we were terminating immediately with
    # last transfer and not servicing it
    n_RLAST = read_channel.reg("n_RLAST", 1)
    # Stores data we want to write to our memory at end of block_transfer group
    read_data_reg = read_channel.reg("read_data_reg", mem["width"])

    bt_reg = read_channel.reg("bt_reg", 1)

    # Groups
    with read_channel.continuous:
        read_channel.this()["RREADY"] = rready.out
        # Tie this low as we are only ever writing to seq_mem
        mem_ref.content_en = 0

    # Wait for handshake. Ensure that when this is done we are ready to write
    # (i.e., read_data_reg.write_en = is_rdy.out)
    # xVALID signals must be high until xREADY is high too, this works because
    # if xREADY is high, then xVALID being high makes 1 flip and group
    # is done by bt_reg.out
    with read_channel.group("block_transfer") as block_transfer:
        RVALID = read_channel.this()["RVALID"]
        RDATA = read_channel.this()["RDATA"]
        RLAST = read_channel.this()["RLAST"]
        # TODO(nathanielnrn): We are allowed to have RREADY depend on RVALID.
        # Can we simplify to just RVALID?

        # rready.in = 1 does not work because it leaves RREADY high for 2 cycles.
        # The way it is below leaves it high for only 1 cycle.  See #1828
        # https://github.com/calyxir/calyx/issues/1828

        # TODO(nathanielnrn): Spec recommends defaulting xREADY high to get rid
        # of extra cycles.  Can we do this as opposed to waiting for RVALID?
        rready.in_ = ~(rready.out & RVALID) @ 1
        rready.in_ = (rready.out & RVALID) @ 0
        rready.write_en = 1

        # Store data we want to write
        read_data_reg.in_ = RDATA
        read_data_reg.write_en = (rready.out & RVALID) @ 1
        read_data_reg.write_en = ~(rready.out & RVALID) @ 0

        n_RLAST.in_ = RLAST @ 0
        n_RLAST.in_ = ~RLAST @ 1
        n_RLAST.write_en = 1

        # We are done after handshake
        bt_reg.in_ = (rready.out & RVALID) @ 1
        bt_reg.in_ = ~(rready.out & RVALID) @ 0
        bt_reg.write_en = 1
        block_transfer.done = bt_reg.out

    with read_channel.group("service_read_transfer") as service_read_transfer:
        # not ready till done servicing
        rready.in_ = 0
        rready.write_en = 1

        # write data we received to mem_ref
        mem_ref.addr0 = curr_addr_internal_mem.out
        mem_ref.write_data = read_data_reg.out
        mem_ref.write_en = 1
        service_read_transfer.done = mem_ref.write_done

    # creates group that increments curr_addr_internal_mem by 1. Creates adder and wires up correctly
    curr_addr_internal_mem_incr = read_channel.incr(curr_addr_internal_mem, 1)
    # TODO(nathanielnrn): Currently we assume that width is a power of 2 due to xSIZE.
    # In the future we should allow for non-power of 2 widths, will need some
    # splicing for this.
    # See https://cucapra.slack.com/archives/C05TRBNKY93/p1705587169286609?thread_ts=1705524171.974079&cid=C05TRBNKY93 # noqa: E501
    curr_addr_axi_incr = read_channel.incr(curr_addr_axi, width_in_bytes(mem["width"]))

    # Control
    invoke_n_RLAST = invoke(n_RLAST, in_in=1)
    invoke_bt_reg = invoke(bt_reg, in_in=0)
    while_body = [
        invoke_bt_reg,
        block_transfer,
        service_read_transfer,
        par(curr_addr_internal_mem_incr, curr_addr_axi_incr),
    ]
    while_n_RLAST = while_(n_RLAST.out, while_body)

    read_channel.control += [invoke_n_RLAST, while_n_RLAST]


def add_write_channel(prog, mem):
    # Inputs/Outputs
    write_channel = prog.component("m_write_channel")
    channel_inputs = [("ARESETn", 1), ("WREADY", 1)]
    # TODO(nathanielnrn): We currently assume WDATA is the same width as the
    # memory. This limits throughput many AXI data busses are much wider
    # i.e., 512 bits.
    channel_outputs = [
        ("WVALID", 1),
        ("WLAST", 1),
        ("WDATA", mem["width"]),
    ]
    add_comp_params(write_channel, channel_inputs, channel_outputs)

    # Cells
    # We assume idx_size is exactly clog2(len). See comment in #1751
    # https://github.com/calyxir/calyx/issues/1751#issuecomment-1778360566
    mem_ref = write_channel.seq_mem_d1(
        name="mem_ref",
        bitwidth=mem["width"],
        len=mem["size"],
        idx_size=clog2(mem["size"]),
        is_external=False,
        is_ref=True,
    )

    # according to zipcpu, rready should be registered
    wvalid = write_channel.reg("wvalid", 1)
    w_handshake_occurred = write_channel.reg("w_handshake_occurred", 1)
    # internal calyx memory indexing
    curr_addr_internal_mem = write_channel.reg(
        "curr_addr_internal_mem", clog2(mem["size"]), is_ref=True
    )
    # host indexing, must be 64 bits
    curr_addr_axi = write_channel.reg("curr_addr_axi", 64, is_ref=True)

    curr_trsnfr_count = write_channel.reg("curr_trsnfr_count", 8)
    # Number of transfers we want to do in current txn
    max_transfers = write_channel.reg("max_transfers", 8, is_ref=True)

    # Register because w last is high with last transfer. Before this
    # We were terminating immediately with last transfer and not servicing it.
    n_finished_last_trnsfr = write_channel.reg("n_finished_last_trnsfr", 1)

    bt_reg = write_channel.reg("bt_reg", 1)

    # Groups
    with write_channel.continuous:
        write_channel.this()["WVALID"] = wvalid.out

    with write_channel.group("service_write_transfer") as service_write_transfer:
        WREADY = write_channel.this()["WREADY"]

        # Assert then deassert. Can maybe getgit right of w_handshake_occurred in guard
        wvalid.in_ = (~(wvalid.out & WREADY) & ~w_handshake_occurred.out) @ 1
        wvalid.in_ = ((wvalid.out & WREADY) | w_handshake_occurred.out) @ 0
        wvalid.write_en = 1

        # Set high when wvalid is high even once
        # This is just wavlid.in_ guard from above
        # TODO: confirm this is correct?
        w_handshake_occurred.in_ = (wvalid.out & WREADY) @ 1
        w_handshake_occurred.write_en = (~w_handshake_occurred.out) @ 1

        # Set data output based on intermal memory output
        mem_ref.addr0 = curr_addr_internal_mem.out
        mem_ref.content_en = 1
        write_channel.this()["WDATA"] = mem_ref.read_data

        write_channel.this()["WLAST"] = (max_transfers.out == curr_trsnfr_count.out) @ 1
        write_channel.this()["WLAST"] = (max_transfers.out != curr_trsnfr_count.out) @ 0

        # set high when WLAST is high and a handshake occurs
        n_finished_last_trnsfr.in_ = (
            (max_transfers.out == curr_trsnfr_count.out) & (wvalid.out & WREADY)
        ) @ 0
        n_finished_last_trnsfr.write_en = (
            (max_transfers.out == curr_trsnfr_count.out) & (wvalid.out & WREADY)
        ) @ 1

        # done after handshake
        bt_reg.in_ = (wvalid.out & WREADY) @ 1
        bt_reg.in_ = ~(wvalid.out & WREADY) @ 0
        bt_reg.write_en = 1
        service_write_transfer.done = bt_reg.out

        # Creates adder and wires up correctly
        curr_addr_internal_mem_incr = write_channel.incr(curr_addr_internal_mem, 1)
        # TODO(nathanielnrn): Currently we assume that width is a power of 2.
        # In the future we should allow for non-power of 2 widths, will need some
        # splicing for this.
        # See https://cucapra.slack.com/archives/C05TRBNKY93/p1705587169286609?thread_ts=1705524171.974079&cid=C05TRBNKY93 # noqa: E501
        curr_addr_axi_incr = write_channel.incr(curr_addr_axi, ceil(mem["width"] / 8))
        curr_trsnfr_count_incr = write_channel.incr(curr_trsnfr_count, 1)

        # Control
        init_curr_addr_internal_mem = invoke(curr_addr_internal_mem, in_in=0)
        init_n_finished_last_trnsfr = invoke(n_finished_last_trnsfr, in_in=1)
        while_n_finished_last_trnsfr_body = [
            invoke(bt_reg, in_in=0),
            service_write_transfer,
            par(
                curr_addr_internal_mem_incr,
                curr_trsnfr_count_incr,
                curr_addr_axi_incr,
                invoke(w_handshake_occurred, in_in=0),
            ),
        ]
        while_n_finished_last_trnsfr = while_(
            n_finished_last_trnsfr.out, while_n_finished_last_trnsfr_body
        )
        write_channel.control += [
            init_curr_addr_internal_mem,
            init_n_finished_last_trnsfr,
            while_n_finished_last_trnsfr,
        ]


# For now we assume all responses are OKAY because we don't have any error
# handling logic. So basically this sets BREADY high then lowers it on
# handshake.
def add_bresp_channel(prog, mem):
    # Inputs/Outputs
    bresp_channel = prog.component("m_bresp_channel")
    # No BRESP because it is ignored, i.e we assume it is tied OKAY
    channel_inputs = [("ARESETn", 1), ("BVALID", 1)]
    channel_outputs = [("BREADY", 1)]
    add_comp_params(bresp_channel, channel_inputs, channel_outputs)

    # Cells
    bready = bresp_channel.reg("bready", 1)
    bt_reg = bresp_channel.reg("bt_reg", 1)

    # Groups
    with bresp_channel.continuous:
        bresp_channel.this()["BREADY"] = bready.out

    # TODO(nathanielnrn): This is probably unoptimal and takes multiple
    # cycles to do a simple handshake which we basically ignore. Can
    # probably be much better.
    with bresp_channel.group("block_transfer") as block_transfer:
        BVALID = bresp_channel.this()["BVALID"]
        bready.in_ = ~(bready.out & BVALID) @ 1
        bready.in_ = (bready.out & BVALID) @ 0
        bready.write_en = 1

        bt_reg.in_ = (bready.out & BVALID) @ 1
        bt_reg.in_ = ~(bready.out & BVALID) @ 0
        bt_reg.write_en = 1
        block_transfer.done = bt_reg.out

    # Control
    bresp_channel.control += [invoke(bt_reg, in_in=0), block_transfer]


# NOTE: Unlike the channel functions, this can expect multiple mems
def add_main_comp(prog, mems):
    wrapper_comp = prog.component("wrapper")
    wrapper_comp.attribute("toplevel",1)
    # Get handles to be used later
    read_channel = prog.get_component("m_read_channel")
    write_channel = prog.get_component("m_write_channel")
    ar_channel = prog.get_component("m_ar_channel")
    aw_channel = prog.get_component("m_aw_channel")
    bresp_channel = prog.get_component("m_bresp_channel")

    curr_addr_axi_par = []
    curr_addr_internal_par = []
    reads_par = []
    writes_par = []
    ref_mem_kwargs = {}

    #Create single main cell
    main_compute = wrapper_comp.comp_instance("main_compute", "main", check_undeclared=False)
    

    for mem in mems:
        mem_name = mem["name"]
        # Inputs/Outputs
        wrapper_inputs = [
                (f"{mem_name}_ARESETn", 1),
                (f"{mem_name}_ARREADY", 1),
                (f"{mem_name}_RVALID", 1),
                (f"{mem_name}_RLAST", 1),
                (f"{mem_name}_RDATA", mem["width"]),
                (f"{mem_name}_RRESP", 2),
                (f"{mem_name}_AWREADY", 1),
                (f"{mem_name}_WRESP", 2),
                (f"{mem_name}_WREADY", 1),
                (f"{mem_name}_BVALID", 1),
                # Only used for waveform tracing, not sent anywhere
                (f"{mem_name}_BRESP", 2),
                # Only needed for coctb compatability, tied low
                (f"{mem_name}_RID", 1),
            ]
        
        wrapper_outputs = [
                (f"{mem_name}_ARVALID", 1),
                (f"{mem_name}_ARADDR", 64),
                (f"{mem_name}_ARSIZE", 3),
                (f"{mem_name}_ARLEN", 8),
                (f"{mem_name}_ARBURST", 2),
                (f"{mem_name}_RREADY", 1),
                (f"{mem_name}_AWVALID", 1),
                (f"{mem_name}_AWADDR", 64),
                (f"{mem_name}_AWSIZE", 3),
                (f"{mem_name}_AWLEN", 8),
                (f"{mem_name}_AWBURST", 2),
                (f"{mem_name}_AWPROT", 3),
                (f"{mem_name}_WVALID", 1),
                (f"{mem_name}_WLAST", 1),
                (f"{mem_name}_WDATA", mem["width"]),
                (f"{mem_name}_BREADY", 1),
                # ID signals are needed for coco compatability, tied low
                (f"{mem_name}_ARID", 1),
                (f"{mem_name}_AWID", 1),
                (f"{mem_name}_WID", 1),
                (f"{mem_name}_BID", 1),
            ]
        

        add_comp_params(wrapper_comp, wrapper_inputs, wrapper_outputs)

        # Cells
        # Read stuff
        curr_addr_internal_mem = wrapper_comp.reg(
            f"curr_addr_internal_mem_{mem_name}", clog2(mem["size"])
        )
        curr_addr_axi = wrapper_comp.reg(f"curr_addr_axi_{mem_name}", 64)

        wrapper_comp.cell(f"ar_channel_{mem_name}", ar_channel)
        wrapper_comp.cell(f"read_channel_{mem_name}", read_channel)

        # TODO: Don't think these need to be marked external, but we
        # we need to raise them at some point form original calyx program
        internal_mem = wrapper_comp.seq_mem_d1(
            name=f"internal_mem_{mem_name}",
            bitwidth=mem["width"],
            len=mem["size"],
            idx_size=clog2(mem["size"]),
        )


        # Write stuff
        max_transfers = wrapper_comp.reg(f"max_transfers_{mem_name}", 8)
        wrapper_comp.cell(f"aw_channel_{mem_name}", aw_channel)
        wrapper_comp.cell(f"write_channel_{mem_name}", write_channel)
        wrapper_comp.cell(f"bresp_channel_{mem_name}", bresp_channel)

        # Wires

        # Tie IDs low, needed for cocotb compatability. Not used anywhere
        with wrapper_comp.continuous:
            wrapper_comp.this()[f"{mem_name}_ARID"] = 0
            wrapper_comp.this()[f"{mem_name}_AWID"] = 0
            wrapper_comp.this()[f"{mem_name}_WID"] = 0
            wrapper_comp.this()[f"{mem_name}_BID"] = 0

        # No groups needed!

        # set up internal control blocks
        #TODO: turn these into parts of a par block
        this_component = wrapper_comp.this()

        ar_channel_invoke = invoke(
            # main_comp.get_cell(f"ar_channel_{mem_name}"),
            wrapper_comp.get_cell(f"ar_channel_{mem_name}"),
            ref_curr_addr_axi=curr_addr_axi,
            in_ARESETn=this_component[f"{mem_name}_ARESETn"],
            in_ARREADY=this_component[f"{mem_name}_ARREADY"],
            out_ARVALID=this_component[f"{mem_name}_ARVALID"],
            out_ARADDR=this_component[f"{mem_name}_ARADDR"],
            out_ARSIZE=this_component[f"{mem_name}_ARSIZE"],
            out_ARLEN=this_component[f"{mem_name}_ARLEN"],
            out_ARBURST=this_component[f"{mem_name}_ARBURST"]
        )

        read_channel_invoke = invoke(
            wrapper_comp.get_cell(f"read_channel_{mem_name}"),
            ref_mem_ref = internal_mem,
            ref_curr_addr_internal_mem = curr_addr_internal_mem,
            ref_curr_addr_axi = curr_addr_axi,
            in_ARESETn = this_component[f"{mem_name}_ARESETn"],
            in_RVALID = this_component[f"{mem_name}_RVALID"],
            in_RLAST = this_component[f"{mem_name}_RLAST"],
            in_RDATA = this_component[f"{mem_name}_RDATA"],
            #TODO: Do we need this? Don't think this goes anywhere
            in_RRESP = this_component[f"{mem_name}_RRESP"],
            out_RREADY = this_component[f"{mem_name}_RREADY"]
        )

        aw_channel_invoke = invoke(
            wrapper_comp.get_cell(f"aw_channel_{mem_name}"),
            ref_curr_addr_axi = curr_addr_axi,
            ref_max_transfers = max_transfers,
            in_ARESETn = this_component[f"{mem_name}_ARESETn"],
            in_AWREADY = this_component[f"{mem_name}_AWREADY"],
            out_AWVALID = this_component[f"{mem_name}_AWVALID"],
            out_AWADDR = this_component[f"{mem_name}_AWADDR"],
            out_AWSIZE = this_component[f"{mem_name}_AWSIZE"],
            out_AWLEN = this_component[f"{mem_name}_AWLEN"],
            out_AWBURST = this_component[f"{mem_name}_AWBURST"],
            out_AWPROT = this_component[f"{mem_name}_AWPROT"]
        )

        write_channel_invoke = invoke(
            wrapper_comp.get_cell(f"write_channel_{mem_name}"),
            ref_mem_ref = internal_mem,
            ref_curr_addr_internal_mem = curr_addr_internal_mem,
            ref_curr_addr_axi = curr_addr_axi,
            ref_max_transfers = max_transfers,
            in_ARESETn = this_component[f"{mem_name}_ARESETn"],
            in_WREADY = this_component[f"{mem_name}_WREADY"],
            out_WVALID = this_component[f"{mem_name}_WVALID"],
            out_WLAST = this_component[f"{mem_name}_WLAST"],
            out_WDATA = this_component[f"{mem_name}_WDATA"],
        )

        bresp_channel_invoke = invoke(
            wrapper_comp.get_cell(f"bresp_channel_{mem_name}"),
            in_BVALID = this_component[f"{mem_name}_BVALID"],
            out_BREADY = this_component[f"{mem_name}_BREADY"]
        )

        curr_addr_axi_invoke = invoke(
            curr_addr_axi, in_in=0x1000
        )
        curr_addr_internal_invoke = invoke(
            curr_addr_internal_mem, in_in=0x0000
        )

        curr_addr_axi_par.append(curr_addr_axi_invoke)
        curr_addr_internal_par.append(curr_addr_internal_invoke)
        reads_par.append([ar_channel_invoke, read_channel_invoke])
        writes_par.append([aw_channel_invoke, write_channel_invoke, bresp_channel_invoke])
        #Creates `<mem_name> = internal_mem_<mem_name>` as refs in invocation of `main_compute`
        ref_mem_kwargs[f"ref_{mem_name}"] = internal_mem


    #Compute invoke
    #Assumes refs should be of form `<mem_name> = internal_mem_<mem_name>`
    main_compute_invoke = invoke(
        wrapper_comp.get_cell("main_compute"),
        **ref_mem_kwargs
    )


    #Compiler should reschedule these 2 seqs to be in parallel right?
    wrapper_comp.control += par(*curr_addr_axi_par)
    wrapper_comp.control += par(*curr_addr_internal_par)

    wrapper_comp.control += par(*reads_par)
    wrapper_comp.control += main_compute_invoke
    # Reset axi adress to 0
    wrapper_comp.control += par(*curr_addr_axi_par)
    wrapper_comp.control += par(*writes_par)

    




# Helper functions
def width_in_bytes(width: int):
    assert width % 8 == 0, "Width must be a multiple of 8."
    return width // 8


def width_xsize(width: int):
    log = log2(width_in_bytes(width))
    assert log.is_integer(), "Width must be a power of 2."
    return int(log)


def clog2(x):
    """Ceiling log2"""
    if x <= 0:
        raise ValueError("x must be positive")
    return (x - 1).bit_length()


def build():
    prog = Builder()
    check_mems_welformed(mems)
    add_arread_channel(prog, mems[0])
    add_awwrite_channel(prog, mems[0])
    add_read_channel(prog, mems[0])
    add_write_channel(prog, mems[0])
    add_bresp_channel(prog, mems[0])
    add_main_comp(prog, mems)
    return prog.program


def check_mems_welformed(mems):
    """Checks if memories from yxi are well formed. Returns true if they are, false otherwise."""
    for mem in mems:
        assert (
            mem["width"] % 8 == 0
        ), "Width must be a multiple of 8 to alow byte addressing to host"
        assert log2(
            mem["width"]
        ).is_integer(), "Width must be a power of 2 to be correctly described by xSIZE"
        assert mem["size"] > 0, "Memory size must be greater than 0"


if __name__ == "__main__":
    build().emit()
