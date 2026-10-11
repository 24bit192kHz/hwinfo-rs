obj-m := zen_extras.o
KDIR ?= /lib/modules/$(shell uname -r)/build
LLVM_FLAG := $(if $(shell grep -s CONFIG_CC_IS_CLANG=y $(KDIR)/.config),LLVM=1)
all:
	$(MAKE) -C $(KDIR) M=$(CURDIR) $(LLVM_FLAG) modules
clean:
	$(MAKE) -C $(KDIR) M=$(CURDIR) $(LLVM_FLAG) clean
